//! qpdf correspondence: QPDFJob::ImageOptimizer and Pl_DCT image compression.
//! qpdf 11.9.0 image-optimization transformation.
//!
//! The implementation follows `QPDFJob::ImageOptimizer` and the surrounding
//! `QPDFJob::handleTransformations` call order (`libqpdf/QPDFJob.cc:36-236,
//! 2137-2174`). Image data is decoded through the canonical specialized stream
//! pipeline, encoded through the qpdf-shaped `Pl_DCT` compression stage, and
//! installed as a deferred stream provider only when the encoded payload is
//! smaller than the original.

use crate::object_handle::ObjectHandle;
use crate::pipeline::count::Count;
use crate::pipeline::dct::PlDct;
use crate::pipeline::{Discard, Pipeline, PlString};
use crate::writer::DecodeLevel;
use crate::{
    Error, ObjectRef, PageDocumentHelper, PageObjectHelper, Pdf, QPDFLogger, Result,
    StreamDataProvider,
};
use fast_image_resize::{
    images::Image as ResizeImage, FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer,
};
use std::collections::HashMap;
use std::io::{Read, Seek};
use std::rc::Rc;

const DEFAULT_OI_MIN_WIDTH: u32 = 128;
const DEFAULT_OI_MIN_HEIGHT: u32 = 128;
const DEFAULT_OI_MIN_AREA: u32 = 16_384;
const DEFAULT_II_MIN_BYTES: usize = 1_024;

/// Options controlling qpdf's `--optimize-images` transformation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageOptimizationOptions {
    /// Minimum image width. Images at or below this width are retained.
    pub min_width: u32,
    /// Minimum image height. Images at or below this height are retained.
    pub min_height: u32,
    /// Minimum image area. Images at or below this area are retained.
    pub min_area: u32,
    /// Minimum encoded inline-image payload for externalization.
    pub inline_min_bytes: usize,
    /// Do not externalize inline images before optimizing XObjects.
    pub keep_inline_images: bool,
    /// JPEG quality used when converting eligible lossless images to DCT.
    /// qpdf-compatible default: 75.
    pub jpeg_quality: u8,
    /// Explicit zlib level used by targeted Flate resize output. `-1` selects
    /// zlib's default; `0..=9` select a concrete level. The qpdf-compatible
    /// non-targeted image optimizer does not use this field.
    pub flate_level: i32,
    /// Require at least this many encoded bytes of savings before replacement.
    pub min_savings_bytes: u64,
    /// Require at least this percentage of encoded savings before replacement.
    pub min_savings_percent: u8,
}

impl Default for ImageOptimizationOptions {
    fn default() -> Self {
        Self {
            min_width: DEFAULT_OI_MIN_WIDTH,
            min_height: DEFAULT_OI_MIN_HEIGHT,
            min_area: DEFAULT_OI_MIN_AREA,
            inline_min_bytes: DEFAULT_II_MIN_BYTES,
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

/// Requested pixel dimensions for an already-placed image XObject.
///
/// The targeted resize API never upscales: dimensions larger than the source
/// are clamped back to the source dimensions before evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageResizeEncoding {
    /// Re-encode resized pixels as JPEG using `ImageOptimizationOptions::jpeg_quality`.
    Jpeg,
    /// Re-encode the resampled pixels with Flate compression. The encoding is
    /// lossless, while the spatial resize itself still discards resolution.
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

struct ResizedImage {
    encoded: Vec<u8>,
    width: u32,
    height: u32,
    original_encoded_bytes: u64,
    encoding: ImageResizeEncoding,
    decode_params: ObjectHandle,
}

impl ImageOptimizationStats {
    pub fn saved_bytes(self) -> u64 {
        self.original_encoded_bytes
            .saturating_sub(self.optimized_encoded_bytes)
    }
}

/// Apply qpdf's image transformation to every page and recursively reachable
/// Form XObject.
///
/// `logger` and `message_prefix` are the job-owned diagnostic route. The
/// transformation itself mutates only the document's canonical ObjectHandle
/// graph; image bytes remain deferred until the writer consumes the new
/// provider-backed stream, matching qpdf's `replaceStreamData` provider path.
pub fn optimize_images<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    logger: &QPDFLogger,
    message_prefix: &str,
    verbose: bool,
    options: ImageOptimizationOptions,
) -> Result<()> {
    optimize_images_with_stats(pdf, logger, message_prefix, verbose, options).map(|_| ())
}

pub fn optimize_images_with_stats<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    logger: &QPDFLogger,
    message_prefix: &str,
    verbose: bool,
    options: ImageOptimizationOptions,
) -> Result<ImageOptimizationStats> {
    let mut stats = ImageOptimizationStats::default();
    let mut optimized_by_source: HashMap<ObjectRef, ObjectHandle> = HashMap::new();
    if !options.keep_inline_images {
        let page_refs = PageDocumentHelper::new(pdf).get_all_pages()?;
        for page_ref in page_refs {
            PageObjectHelper::new(page_ref, pdf)
                .externalize_inline_images(options.inline_min_bytes, false)?;
        }
    }

    let page_refs = PageDocumentHelper::new(pdf).get_all_pages()?;
    for (page_index, page_ref) in page_refs.into_iter().enumerate() {
        let page_number = page_index + 1;
        let mut replacements = Vec::new();
        {
            let mut page = PageObjectHelper::new(page_ref, pdf);
            page.for_each_image(true, |image, xobjects, key| {
                let description = format!(
                    "image {} on page {page_number}",
                    String::from_utf8_lossy(&key)
                );
                if let Some(source_ref) = image.object_ref() {
                    if let Some(cached) = optimized_by_source.get(&source_ref) {
                        replacements.push((xobjects, key, cached.clone()));
                        stats.references_reused += 1;
                        return Ok(());
                    }
                }
                if !ImageOptimizer::preflight(&image)? {
                    log_skip(
                        logger,
                        message_prefix,
                        verbose,
                        &description,
                        SkipReason::UnableToDecode,
                    )?; // cov:ignore: llvm-cov attributes this successful multiline logger call to its opening expressions
                    return Ok(());
                }
                let optimizer = match ImageOptimizer::prepare(image.clone(), options)? {
                    PrepareResult::Ready(optimizer) => optimizer,
                    PrepareResult::Skip(reason) => {
                        log_skip(logger, message_prefix, verbose, &description, reason)?;
                        return Ok(());
                    }
                };

                let Some(evaluation) = optimizer.evaluate()? else {
                    return Ok(()); // cov:ignore: the preflight and evaluation use the same immutable source; only an external mutable provider can make the second pipe fail
                };
                match evaluation {
                    Evaluation::NotSmaller => {
                        log_verbose(
                            logger,
                            message_prefix,
                            verbose,
                            &description,
                            "not optimizing because DCT compression does not reduce image size"
                                .to_owned(),
                        )?; // cov:ignore: llvm-cov attributes this successful multiline logger call to its opening expressions
                    }
                    Evaluation::BelowSavingsThreshold => {
                        log_verbose(
                            logger,
                            message_prefix,
                            verbose,
                            &description,
                            "not optimizing because DCT compression savings are below the configured threshold"
                                .to_owned(),
                        )?;
                    }
                    Evaluation::Smaller {
                        original_length,
                        compressed_length,
                    } => {
                        log_verbose(
                            logger,
                            message_prefix,
                            verbose,
                            &description,
                            format!(
                                "optimizing image reduces size from {original_length} to {compressed_length}"
                            ),
                        )?; // cov:ignore: llvm-cov attributes this successful multiline logger call to its opening expressions

                        stats.images_optimized += 1;
                        stats.original_encoded_bytes += original_length;
                        stats.optimized_encoded_bytes += compressed_length;

                        let source_ref = image.object_ref();
                        let new_image = image.copy_stream()?;
                        new_image.replace_stream_data_provider(
                            Rc::new(optimizer),
                            Some(ObjectHandle::name(b"DCTDecode".to_vec())),
                            Some(ObjectHandle::null()),
                        )?; // cov:ignore: provider registration succeeds for the freshly copied indirect stream
                        if let Some(source_ref) = source_ref {
                            optimized_by_source.insert(source_ref, new_image.clone());
                        }
                        replacements.push((xobjects, key, new_image));
                    }
                }
                Ok(())
            })?;
        }

        for (xobjects, key, new_image) in replacements {
            xobjects.replace_key(&key, new_image.clone())?;
            pdf.mark_object_handle_dirty(&new_image)?;
            pdf.mark_object_handle_dirty(&xobjects)?;
        }
    }
    Ok(stats)
}

fn is_conservative_dct_resize_source(dictionary: &ObjectHandle) -> Result<bool> {
    if !dictionary.try_get_key(b"/SMask")?.is_null()
        || !dictionary.try_get_key(b"/Mask")?.is_null()
        || !dictionary.try_get_key(b"/Decode")?.is_null()
        || !dictionary.try_get_key(b"/DecodeParms")?.is_null()
        || dictionary
            .try_get_key(b"/BitsPerComponent")?
            .try_get_int_value()
            .ok()
            != Some(8)
    {
        return Ok(false);
    }

    let filter = dictionary.try_get_key(b"/Filter")?;
    if !filter.try_is_name_and_equals(b"DCTDecode")? && !filter.try_is_name_and_equals(b"DCT")? {
        return Ok(false);
    }

    let color_space = dictionary.try_get_key(b"/ColorSpace")?;
    color_space.try_dereference()?;
    Ok(color_space.try_is_name_and_equals(b"DeviceGray")?
        || color_space.try_is_name_and_equals(b"DeviceRGB")?)
}

fn is_conservative_flate_resize_source(dictionary: &ObjectHandle) -> Result<bool> {
    if !dictionary.try_get_key(b"/SMask")?.is_null()
        || !dictionary.try_get_key(b"/Mask")?.is_null()
        || !dictionary.try_get_key(b"/Decode")?.is_null()
        || dictionary
            .try_get_key(b"/BitsPerComponent")?
            .try_get_int_value()
            .ok()
            != Some(8)
    {
        return Ok(false);
    }

    let filter = dictionary.try_get_key(b"/Filter")?;
    if !filter.try_is_name_and_equals(b"FlateDecode")? && !filter.try_is_name_and_equals(b"Fl")? {
        return Ok(false);
    }

    let color_space = dictionary.try_get_key(b"/ColorSpace")?;
    color_space.try_dereference()?;
    Ok(color_space.try_is_name_and_equals(b"DeviceGray")?
        || color_space.try_is_name_and_equals(b"DeviceRGB")?)
}

fn isolate_page_xobjects<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    page_ref: ObjectRef,
) -> Result<ObjectHandle> {
    let resources = PageObjectHelper::new(page_ref, pdf).get_resources(true)?;
    if resources.is_null() {
        return Err(Error::Internal(
            "targeted image replacement page has no resources".to_owned(),
        ));
    }
    let xobjects = resources.try_get_key(b"/XObject")?;
    pdf.resolve(&xobjects)?;
    if xobjects.as_dictionary().is_none() {
        return Err(Error::Internal(
            "targeted image replacement page has no XObject dictionary".to_owned(),
        ));
    }

    let isolated = xobjects.shallow_copy()?;
    resources.replace_key(b"/XObject", isolated.clone())?;
    pdf.mark_object_handle_dirty(&resources)?;
    Ok(isolated)
}

/// Resize selected image XObjects to caller-supplied pixel dimensions and
/// re-encode accepted results as JPEG. Images not present in `targets` are
/// untouched. The target map is keyed by `(page, source image)` identity, so
/// only page bindings explicitly selected by a placement-aware caller are
/// mutated. Page resource dictionaries are copy-on-write isolated before
/// replacement, and a shared source image with the same target dimensions is
/// transformed once and reused across selected page bindings.
///
/// This is intentionally separate from qpdf-compatible [`optimize_images`]:
/// existing callers retain qpdf's no-resize behavior, while placement-aware
/// consumers can opt into dimension changes explicitly.
pub fn optimize_images_with_resize_targets<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
    logger: &QPDFLogger,
    message_prefix: &str,
    verbose: bool,
    options: ImageOptimizationOptions,
    targets: &HashMap<(ObjectRef, ObjectRef), ImageResizeTarget>,
) -> Result<ImageOptimizationStats> {
    let mut stats = ImageOptimizationStats::default();
    let mut optimized_by_source: HashMap<(ObjectRef, ImageResizeTarget), ObjectHandle> =
        HashMap::new();
    let page_refs = PageDocumentHelper::new(pdf).get_all_pages()?;

    for (page_index, page_ref) in page_refs.into_iter().enumerate() {
        let page_number = page_index + 1;
        let mut replacements = Vec::new();
        {
            let mut page = PageObjectHelper::new(page_ref, pdf);
            page.for_each_image(false, |image, _xobjects, key| {
                let Some(source_ref) = image.object_ref() else {
                    return Ok(());
                };
                let Some(target) = targets.get(&(page_ref, source_ref)).copied() else {
                    return Ok(());
                };
                let description = format!(
                    "image {} on page {page_number}",
                    String::from_utf8_lossy(&key)
                );

                if let Some(cached) = optimized_by_source.get(&(source_ref, target)) {
                    replacements.push((key, cached.clone()));
                    stats.references_reused += 1;
                    return Ok(());
                }

                let Some(dictionary) = image.as_stream_dict() else {
                    return Ok(());
                };
                let conservative_source = match target.encoding {
                    ImageResizeEncoding::Jpeg => is_conservative_dct_resize_source(&dictionary),
                    ImageResizeEncoding::Flate => {
                        is_conservative_flate_resize_source(&dictionary)
                    }
                };
                let conservative_source = match conservative_source {
                    Ok(value) => value,
                    Err(error) => {
                        log_verbose(
                            logger,
                            message_prefix,
                            verbose,
                            &description,
                            format!("not resizing because image metadata is malformed: {error}"),
                        )?;
                        return Ok(());
                    }
                };
                if !conservative_source {
                    log_verbose(
                        logger,
                        message_prefix,
                        verbose,
                        &description,
                        "not resizing because the source uses unsupported color, mask, decode, or filter semantics"
                            .to_owned(),
                    )?;
                    return Ok(());
                }

                let optimizer = match ImageOptimizer::prepare(image.clone(), options) {
                    Ok(PrepareResult::Ready(optimizer)) => optimizer,
                    Ok(PrepareResult::Skip(reason)) => {
                        log_skip(logger, message_prefix, verbose, &description, reason)?;
                        return Ok(());
                    }
                    Err(error) => {
                        log_verbose(
                            logger,
                            message_prefix,
                            verbose,
                            &description,
                            format!("not resizing because image metadata is malformed: {error}"),
                        )?;
                        return Ok(());
                    }
                };

                let resized = match optimizer.resize_and_encode(target) {
                    Ok(Some(resized)) => resized,
                    Ok(None) => return Ok(()),
                    Err(error) => {
                        log_verbose(
                            logger,
                            message_prefix,
                            verbose,
                            &description,
                            format!(
                                "not resizing because image decoding/resampling failed: {error}"
                            ),
                        )?;
                        return Ok(());
                    }
                };
                let original_length = resized.original_encoded_bytes;
                let compressed_length = resized.encoded.len() as u64;
                let savings = original_length.saturating_sub(compressed_length);
                let clears_byte_gate = savings >= options.min_savings_bytes;
                let clears_percent_gate = u128::from(savings) * 100
                    >= u128::from(original_length) * u128::from(options.min_savings_percent);
                if compressed_length >= original_length || !clears_byte_gate || !clears_percent_gate
                {
                    return Ok(());
                }

                let new_image = image.copy_stream()?;
                let new_dictionary = new_image.as_stream_dict().ok_or_else(|| {
                    Error::Internal("copied image has no stream dictionary".to_owned())
                })?;
                new_dictionary
                    .replace_key(b"/Width", ObjectHandle::integer(i64::from(resized.width)))?;
                new_dictionary
                    .replace_key(b"/Height", ObjectHandle::integer(i64::from(resized.height)))?;
                let output_filter = match resized.encoding {
                    ImageResizeEncoding::Jpeg => b"DCTDecode".as_slice(),
                    ImageResizeEncoding::Flate => b"FlateDecode".as_slice(),
                };
                new_image.replace_stream_data(
                    Rc::new(resized.encoded),
                    Some(ObjectHandle::name(output_filter.to_vec())),
                    Some(resized.decode_params),
                );
                // These are already the final encoded bytes whose size was
                // evaluated above. Keep the writer from decoding/recompressing
                // them again under document-wide stream policies.
                new_image.set_filter_on_write(false)?;

                stats.images_optimized += 1;
                stats.images_resized += 1;
                match resized.encoding {
                    ImageResizeEncoding::Jpeg => stats.jpeg_images_resized += 1,
                    ImageResizeEncoding::Flate => stats.flate_images_resized += 1,
                }
                stats.original_encoded_bytes += original_length;
                stats.optimized_encoded_bytes += compressed_length;
                stats.original_pixels += u64::from(optimizer.width) * u64::from(optimizer.height);
                stats.optimized_pixels += u64::from(resized.width) * u64::from(resized.height);

                optimized_by_source.insert((source_ref, target), new_image.clone());
                replacements.push((key, new_image));
                Ok(())
            })?;
        }

        if !replacements.is_empty() {
            let xobjects = isolate_page_xobjects(pdf, page_ref)?;
            for (key, new_image) in replacements {
                xobjects.replace_key(&key, new_image.clone())?;
                pdf.mark_object_handle_dirty(&new_image)?;
            }
            pdf.mark_object_handle_dirty(&xobjects)?;
        }
    }
    Ok(stats)
}

fn log_skip(
    logger: &QPDFLogger,
    message_prefix: &str,
    verbose: bool,
    description: &str,
    reason: SkipReason,
) -> Result<()> {
    log_verbose(
        logger,
        message_prefix,
        verbose,
        description,
        reason.message().to_owned(),
    )
}

fn log_verbose(
    logger: &QPDFLogger,
    message_prefix: &str,
    verbose: bool,
    description: &str,
    message: String,
) -> Result<()> {
    if verbose {
        logger.info(format!("{message_prefix}: {description}: {message}\n"))?;
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum SkipReason {
    MissingKeys,
    BitsPerComponent,
    Colorspace,
    TooSmall,
    UnableToDecode,
}

impl SkipReason {
    fn message(self) -> &'static str {
        match self {
            Self::MissingKeys => "not optimizing because image dictionary is missing required keys",
            Self::BitsPerComponent => {
                "not optimizing because image has other than 8 bits per component"
            }
            Self::Colorspace => {
                "not optimizing because qpdf can't optimize images with this colorspace"
            }
            Self::TooSmall => {
                "not optimizing because image is smaller than requested minimum dimensions"
            }
            Self::UnableToDecode => {
                "not optimizing because unable to decode data or data already uses DCT"
            }
        }
    }
}

enum PrepareResult {
    Ready(ImageOptimizer),
    Skip(SkipReason),
}

enum Evaluation {
    NotSmaller,
    BelowSavingsThreshold,
    Smaller {
        original_length: u64,
        compressed_length: u64,
    },
}

struct ImageOptimizer {
    image: ObjectHandle,
    dictionary: ObjectHandle,
    width: u32,
    height: u32,
    pixel_format: libjpeg_turbo_rs::PixelFormat,
    jpeg_quality: u8,
    flate_level: i32,
    min_savings_bytes: u64,
    min_savings_percent: u8,
}

impl ImageOptimizer {
    fn preflight(image: &ObjectHandle) -> Result<bool> {
        let mut discard = Discard;
        let mut filtering_attempted = false;
        let succeeded = image.pipe_stream_data(
            &mut discard,
            &mut filtering_attempted,
            0,
            DecodeLevel::Specialized,
            true,
            false,
        )?; // cov:ignore: the preflight pipeline call is the qpdf filterability probe; its successful continuation is attributed to the opening expressions
        Ok(succeeded && filtering_attempted)
    }

    fn prepare(image: ObjectHandle, options: ImageOptimizationOptions) -> Result<PrepareResult> {
        let dictionary = image.as_stream_dict().ok_or_else(|| {
            crate::Error::Internal("image XObject has no stream dictionary".into())
        })?;
        let width_value = dictionary.try_get_key(b"/Width")?;
        let height_value = dictionary.try_get_key(b"/Height")?;
        if !width_value.try_is_number()? || !height_value.try_is_number()? {
            return Ok(PrepareResult::Skip(SkipReason::MissingKeys));
        }
        let width = qpdf_dimension(&width_value)?;
        let height = qpdf_dimension(&height_value)?;

        let bits_per_component = dictionary.try_get_key(b"/BitsPerComponent")?;
        if !bits_per_component.try_is_integer()? || bits_per_component.try_get_int_value()? != 8 {
            return Ok(PrepareResult::Skip(SkipReason::BitsPerComponent));
        }

        let colorspace = dictionary.try_get_key(b"/ColorSpace")?;
        colorspace.try_dereference()?;
        let Some(colorspace) = colorspace.as_name() else {
            return Ok(PrepareResult::Skip(SkipReason::Colorspace));
        };
        let pixel_format = match colorspace.as_slice() {
            b"DeviceRGB" => libjpeg_turbo_rs::PixelFormat::Rgb,
            b"DeviceGray" => libjpeg_turbo_rs::PixelFormat::Grayscale,
            b"DeviceCMYK" => libjpeg_turbo_rs::PixelFormat::Cmyk,
            _ => return Ok(PrepareResult::Skip(SkipReason::Colorspace)),
        };

        let area = width.wrapping_mul(height);
        if (options.min_width > 0 && width <= options.min_width)
            || (options.min_height > 0 && height <= options.min_height)
            || (options.min_area > 0 && area <= options.min_area)
        {
            return Ok(PrepareResult::Skip(SkipReason::TooSmall));
        }

        Ok(PrepareResult::Ready(Self {
            image,
            dictionary,
            width,
            height,
            pixel_format,
            jpeg_quality: options.jpeg_quality,
            flate_level: options.flate_level,
            min_savings_bytes: options.min_savings_bytes,
            min_savings_percent: options.min_savings_percent,
        }))
    }

    fn evaluate(&self) -> Result<Option<Evaluation>> {
        let mut discard = Discard;
        let mut count = Count::new("count", &mut discard);
        let mut encoder = self.encoder(&mut count);
        let mut filtering_attempted = false;
        let succeeded = self.image.pipe_stream_data(
            &mut encoder,
            &mut filtering_attempted,
            0,
            DecodeLevel::Specialized,
            false,
            false,
        )?; // cov:ignore: the second pipe is over the same immutable source already accepted by preflight; only an external mutable provider can return false here
        drop(encoder);
        if !succeeded {
            return Ok(None); // cov:ignore: StreamDataProvider requires stable bytes across calls; preflight already accepted this source
        }

        let original_length = self
            .dictionary
            .try_get_key(b"/Length")?
            .try_get_int_value()?
            .max(0) as u64;
        let compressed_length = count.count();
        let savings = original_length.saturating_sub(compressed_length);
        let clears_byte_gate = savings >= self.min_savings_bytes;
        let clears_percent_gate = u128::from(savings) * 100
            >= u128::from(original_length) * u128::from(self.min_savings_percent);
        if compressed_length >= original_length {
            Ok(Some(Evaluation::NotSmaller))
        } else if !clears_byte_gate || !clears_percent_gate {
            Ok(Some(Evaluation::BelowSavingsThreshold))
        } else {
            Ok(Some(Evaluation::Smaller {
                original_length,
                compressed_length,
            }))
        }
    }

    fn encoder<'a>(&self, next: &'a mut dyn Pipeline) -> PlDct<'a> {
        self.encoder_for_dimensions(next, self.width, self.height)
    }

    fn encoder_for_dimensions<'a>(
        &self,
        next: &'a mut dyn Pipeline,
        width: u32,
        height: u32,
    ) -> PlDct<'a> {
        if self.jpeg_quality == 75 {
            PlDct::new_compressor(
                "jpg",
                next,
                width as usize,
                height as usize,
                self.pixel_format,
            )
        } else {
            PlDct::new_compressor_with_quality(
                "jpg",
                next,
                width as usize,
                height as usize,
                self.pixel_format,
                self.jpeg_quality,
            )
        }
    }

    fn resize_pixel_type(&self) -> Option<PixelType> {
        match self.pixel_format {
            libjpeg_turbo_rs::PixelFormat::Grayscale => Some(PixelType::U8),
            libjpeg_turbo_rs::PixelFormat::Rgb => Some(PixelType::U8x3),
            libjpeg_turbo_rs::PixelFormat::Cmyk => Some(PixelType::U8x4),
            _ => None,
        }
    }

    fn resize_and_encode(&self, target: ImageResizeTarget) -> Result<Option<ResizedImage>> {
        let target_width = target.width.max(1).min(self.width);
        let target_height = target.height.max(1).min(self.height);
        if target_width == self.width && target_height == self.height {
            return Ok(None);
        }

        let raw = self.image.get_raw_stream_data()?;
        let original_encoded_bytes = raw.len() as u64;
        let decoded = crate::filters::decode_stream_data(&self.dictionary, raw.as_ref())?;
        let pixel_type = self.resize_pixel_type().ok_or_else(|| {
            Error::Unsupported("unsupported pixel format for image resizing".to_owned())
        })?;
        let source = ResizeImage::from_vec_u8(self.width, self.height, decoded, pixel_type)
            .map_err(|error| {
                Error::Unsupported(format!("invalid decoded image buffer: {error}"))
            })?;
        let mut destination = ResizeImage::new(target_width, target_height, pixel_type);
        let mut resizer = Resizer::new();
        let resize_options =
            ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
        resizer
            .resize(&source, &mut destination, &resize_options)
            .map_err(|error| Error::Unsupported(format!("unable to resize image: {error}")))?;

        let (encoded, decode_params) = match target.encoding {
            ImageResizeEncoding::Jpeg => {
                let mut encoded = Vec::new();
                {
                    let mut sink = PlString::new("resized jpeg", None, &mut encoded);
                    let mut encoder =
                        self.encoder_for_dimensions(&mut sink, target_width, target_height);
                    encoder.write(destination.buffer())?;
                    encoder.finish()?;
                }
                (encoded, ObjectHandle::null())
            }
            ImageResizeEncoding::Flate => {
                let colors = match self.pixel_format {
                    libjpeg_turbo_rs::PixelFormat::Grayscale => 1,
                    libjpeg_turbo_rs::PixelFormat::Rgb => 3,
                    _ => {
                        return Err(Error::Unsupported(
                            "Flate resize output supports only Gray/RGB pixels".to_owned(),
                        ));
                    }
                };
                let decode_params = ObjectHandle::dictionary(vec![
                    (b"/Predictor".to_vec(), ObjectHandle::integer(12)),
                    (b"/Colors".to_vec(), ObjectHandle::integer(colors)),
                    (b"/BitsPerComponent".to_vec(), ObjectHandle::integer(8)),
                    (
                        b"/Columns".to_vec(),
                        ObjectHandle::integer(i64::from(target_width)),
                    ),
                ]);
                let output_dictionary = ObjectHandle::dictionary(vec![
                    (
                        b"/Filter".to_vec(),
                        ObjectHandle::name(b"FlateDecode".to_vec()),
                    ),
                    (b"/DecodeParms".to_vec(), decode_params.clone()),
                ]);
                let encoded = crate::filters::encode_stream_data_with_flate_level(
                    &output_dictionary,
                    destination.buffer(),
                    self.flate_level,
                )?;
                (encoded, decode_params)
            }
        };
        Ok(Some(ResizedImage {
            encoded,
            width: target_width,
            height: target_height,
            original_encoded_bytes,
            encoding: target.encoding,
            decode_params,
        }))
    }
}

impl StreamDataProvider for ImageOptimizer {
    fn provide_stream_data_by_id(
        &self,
        _object_number: u32,
        _generation: u16,
        pipeline: &mut dyn Pipeline,
    ) -> Result<()> {
        let mut encoder = self.encoder(pipeline);
        let mut filtering_attempted = false;
        let _ = self.image.pipe_stream_data(
            &mut encoder,
            &mut filtering_attempted,
            0,
            DecodeLevel::Specialized,
            false,
            false,
        )?; // cov:ignore: provider output is consumed by the writer's already-valid downstream pipeline
        Ok(())
    }
}

fn qpdf_dimension(value: &ObjectHandle) -> Result<u32> {
    if value.try_is_integer()? {
        let value = value.try_get_int_value()?;
        return Ok(value.clamp(0, i64::from(u32::MAX)) as u32);
    }
    let value = value.try_get_numeric_value()?;
    if value.is_nan() {
        return Ok(0);
    }
    Ok(value.clamp(0.0, f64::from(u32::MAX)) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::PlString;
    use std::rc::Rc;

    fn image_dictionary(width: ObjectHandle, height: ObjectHandle) -> ObjectHandle {
        ObjectHandle::dictionary(vec![
            (b"/Width".to_vec(), width),
            (b"/Height".to_vec(), height),
            (b"/BitsPerComponent".to_vec(), ObjectHandle::integer(8)),
            (
                b"/ColorSpace".to_vec(),
                ObjectHandle::name(b"DeviceGray".to_vec()),
            ),
            (b"/Length".to_vec(), ObjectHandle::integer(0)),
        ])
    }

    fn direct_image(width: usize, height: usize, data: Vec<u8>) -> ObjectHandle {
        let dictionary = image_dictionary(
            ObjectHandle::integer(width as i64),
            ObjectHandle::integer(height as i64),
        );
        dictionary
            .replace_key(b"/Length", ObjectHandle::integer(data.len() as i64))
            .unwrap();
        ObjectHandle::stream(dictionary, Rc::new(data))
    }

    fn prepared_gray_optimizer(width: usize, height: usize, data: Vec<u8>) -> ImageOptimizer {
        let image = direct_image(width, height, data);
        let dictionary = image.as_stream_dict().expect("image dictionary");
        ImageOptimizer {
            image,
            dictionary,
            width: width as u32,
            height: height as u32,
            pixel_format: libjpeg_turbo_rs::PixelFormat::Grayscale,
            jpeg_quality: 75,
            flate_level: -1,
            min_savings_bytes: 1,
            min_savings_percent: 0,
        }
    }

    #[test]
    fn prepare_matches_qpdf_metadata_skip_reasons_and_numeric_dimensions() {
        assert!(matches!(
            ImageOptimizer::prepare(
                ObjectHandle::stream(
                    ObjectHandle::dictionary(vec![(
                        b"/Width".to_vec(),
                        ObjectHandle::integer(200),
                    )]),
                    Rc::new(Vec::new()),
                ),
                ImageOptimizationOptions::default(),
            )
            .unwrap(),
            PrepareResult::Skip(SkipReason::MissingKeys)
        ));

        let bad_bits = ObjectHandle::stream(
            ObjectHandle::dictionary(vec![
                (b"/Width".to_vec(), ObjectHandle::integer(200)),
                (b"/Height".to_vec(), ObjectHandle::integer(200)),
                (b"/BitsPerComponent".to_vec(), ObjectHandle::integer(1)),
            ]),
            Rc::new(Vec::new()),
        );
        assert!(matches!(
            ImageOptimizer::prepare(bad_bits, ImageOptimizationOptions::default()).unwrap(),
            PrepareResult::Skip(SkipReason::BitsPerComponent)
        ));

        let bad_colorspace = ObjectHandle::stream(
            ObjectHandle::dictionary(vec![
                (b"/Width".to_vec(), ObjectHandle::integer(200)),
                (b"/Height".to_vec(), ObjectHandle::integer(200)),
                (b"/BitsPerComponent".to_vec(), ObjectHandle::integer(8)),
                (
                    b"/ColorSpace".to_vec(),
                    ObjectHandle::name(b"Pattern".to_vec()),
                ),
            ]),
            Rc::new(Vec::new()),
        );
        assert!(matches!(
            ImageOptimizer::prepare(bad_colorspace, ImageOptimizationOptions::default()).unwrap(),
            PrepareResult::Skip(SkipReason::Colorspace)
        ));

        let small_options = ImageOptimizationOptions {
            min_width: 200,
            ..ImageOptimizationOptions::default()
        };
        let small = ObjectHandle::stream(
            image_dictionary(ObjectHandle::integer(200), ObjectHandle::integer(200)),
            Rc::new(Vec::new()),
        );
        assert!(matches!(
            ImageOptimizer::prepare(small, small_options).unwrap(),
            PrepareResult::Skip(SkipReason::TooSmall)
        ));

        assert_eq!(qpdf_dimension(&ObjectHandle::real(200.9)).unwrap(), 200);
        assert_eq!(qpdf_dimension(&ObjectHandle::real(f64::NAN)).unwrap(), 0);
        assert_eq!(qpdf_dimension(&ObjectHandle::integer(-1)).unwrap(), 0);
        assert_eq!(
            qpdf_dimension(&ObjectHandle::integer(i64::MAX)).unwrap(),
            u32::MAX
        );
    }

    #[test]
    fn prepare_rejects_a_non_stream_image_handle() {
        let result = ImageOptimizer::prepare(
            ObjectHandle::integer(1),
            ImageOptimizationOptions::default(),
        );
        assert!(
            matches!(result, Err(crate::Error::Internal(message)) if message == "image XObject has no stream dictionary")
        );
    }

    #[test]
    fn optimizer_preflight_evaluate_and_provider_share_the_same_source() {
        let image = direct_image(200, 200, vec![128; 40_000]);
        assert!(ImageOptimizer::preflight(&image).unwrap());
        let optimizer = prepared_gray_optimizer(200, 200, vec![128; 40_000]);
        assert!(matches!(
            optimizer.evaluate().unwrap(),
            Some(Evaluation::Smaller { .. })
        ));

        let mut output = Vec::new();
        let mut sink = PlString::new("sink", None, &mut output);
        optimizer
            .provide_stream_data_by_id(0, 0, &mut sink)
            .expect("provider emits the same deterministic JPEG");
        assert!(output.starts_with(&[0xff, 0xd8]));
    }

    #[test]
    fn targeted_resize_isolates_page_xobjects_from_other_resource_owners() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/compat/direct-root-one-page.pdf");
        let mut pdf = Pdf::open_mem_owned(std::fs::read(path).expect("fixture exists"))
            .expect("fixture opens");
        let page_ref = crate::pages::page_refs(&mut pdf).expect("page refs")[0];

        let shared_xobjects = pdf
            .make_indirect_object_handle(ObjectHandle::dictionary(vec![(
                b"/Im".to_vec(),
                ObjectHandle::integer(7),
            )]))
            .expect("shared XObject dictionary");
        let shared_resources = pdf
            .make_indirect_object_handle(ObjectHandle::dictionary(vec![(
                b"/XObject".to_vec(),
                shared_xobjects.clone(),
            )]))
            .expect("shared Resources dictionary");

        let page = pdf.get_object_handle(page_ref);
        pdf.resolve(&page).expect("page resolves");
        page.replace_key(b"/Resources", shared_resources.clone())
            .expect("page resources");
        pdf.mark_object_handle_dirty(&page).expect("dirty page");

        let other_owner = pdf
            .make_indirect_object_handle(ObjectHandle::dictionary(vec![(
                b"/Resources".to_vec(),
                shared_resources,
            )]))
            .expect("other resource owner");

        let isolated = isolate_page_xobjects(&mut pdf, page_ref).expect("isolate page XObjects");
        isolated
            .replace_key(b"/Im", ObjectHandle::integer(8))
            .expect("mutate page-local XObject dictionary");

        pdf.resolve(&other_owner).expect("other owner resolves");
        let other_resources = other_owner
            .try_get_key(b"/Resources")
            .expect("other resources");
        pdf.resolve(&other_resources)
            .expect("other resources resolve");
        let other_xobjects = other_resources
            .try_get_key(b"/XObject")
            .expect("other XObjects");
        pdf.resolve(&other_xobjects)
            .expect("other XObjects resolve");

        assert_eq!(
            isolated.try_get_key(b"/Im").expect("page Im").as_integer(),
            Some(8)
        );
        assert_eq!(
            other_xobjects
                .try_get_key(b"/Im")
                .expect("other Im")
                .as_integer(),
            Some(7)
        );
    }

    #[test]
    fn targeted_resize_rejects_ambiguous_pdf_image_semantics() {
        let image = direct_image(200, 100, vec![0; 20_000]);
        let dictionary = image.as_stream_dict().expect("image dictionary");
        dictionary
            .replace_key(b"/Filter", ObjectHandle::name(b"DCTDecode".to_vec()))
            .expect("install DCT filter");
        assert!(is_conservative_dct_resize_source(&dictionary).unwrap());

        dictionary
            .replace_key(b"/ColorSpace", ObjectHandle::name(b"DeviceCMYK".to_vec()))
            .expect("install CMYK color space");
        assert!(!is_conservative_dct_resize_source(&dictionary).unwrap());

        dictionary
            .replace_key(b"/ColorSpace", ObjectHandle::name(b"DeviceRGB".to_vec()))
            .expect("restore RGB color space");
        dictionary
            .replace_key(
                b"/Decode",
                ObjectHandle::array(vec![
                    ObjectHandle::integer(1),
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(1),
                    ObjectHandle::integer(0),
                    ObjectHandle::integer(1),
                    ObjectHandle::integer(0),
                ]),
            )
            .expect("install decode array");
        assert!(!is_conservative_dct_resize_source(&dictionary).unwrap());

        dictionary
            .replace_key(b"/Decode", ObjectHandle::null())
            .expect("clear decode array");
        dictionary
            .replace_key(b"/Mask", ObjectHandle::array(Vec::new()))
            .expect("install mask");
        assert!(!is_conservative_dct_resize_source(&dictionary).unwrap());
    }

    #[test]
    fn resize_path_decodes_existing_dct_and_reencodes_target_dimensions() {
        let source_pixels: Vec<u8> = (0..200 * 100)
            .map(|index| ((index * 37 + index / 11) & 0xff) as u8)
            .collect();
        let source_optimizer = prepared_gray_optimizer(200, 100, source_pixels);
        let mut jpeg = Vec::new();
        {
            let mut sink = PlString::new("jpeg sink", None, &mut jpeg);
            source_optimizer
                .provide_stream_data_by_id(0, 0, &mut sink)
                .expect("encode source JPEG");
        }

        let image = direct_image(200, 100, jpeg);
        let dictionary = image.as_stream_dict().expect("image dictionary");
        dictionary
            .replace_key(b"/Filter", ObjectHandle::name(b"DCTDecode".to_vec()))
            .expect("install DCT filter");
        let optimizer = match ImageOptimizer::prepare(
            image,
            ImageOptimizationOptions {
                min_width: 0,
                min_height: 0,
                min_area: 0,
                jpeg_quality: 85,
                ..ImageOptimizationOptions::default()
            },
        )
        .expect("prepare DCT resize source")
        {
            PrepareResult::Ready(optimizer) => optimizer,
            PrepareResult::Skip(_) => panic!("DCT resize source should be eligible"),
        };

        let resized = optimizer
            .resize_and_encode(ImageResizeTarget::jpeg(100, 50))
            .expect("resize DCT source")
            .expect("target is smaller than source");
        assert_eq!((resized.width, resized.height), (100, 50));
        assert!(resized.encoded.starts_with(&[0xff, 0xd8]));

        let resized_image = direct_image(100, 50, resized.encoded);
        let resized_dict = resized_image.as_stream_dict().expect("resized dictionary");
        resized_dict
            .replace_key(b"/Filter", ObjectHandle::name(b"DCTDecode".to_vec()))
            .expect("install resized DCT filter");
        let decoded = crate::filters::decode_stream_data(
            &resized_dict,
            resized_image
                .get_raw_stream_data()
                .expect("read resized JPEG")
                .as_ref(),
        )
        .expect("decode resized JPEG");
        assert_eq!(decoded.len(), 100 * 50);
    }

    #[test]
    fn resize_path_decodes_predictor_flate_and_keeps_flate_encoding() {
        let width = 200usize;
        let height = 100usize;
        let pixels: Vec<u8> = (0..width * height)
            .map(|index| ((index * 17 + index / 13) & 0xff) as u8)
            .collect();
        let dictionary = image_dictionary(
            ObjectHandle::integer(width as i64),
            ObjectHandle::integer(height as i64),
        );
        dictionary
            .replace_key(b"/Filter", ObjectHandle::name(b"FlateDecode".to_vec()))
            .expect("install Flate filter");
        dictionary
            .replace_key(
                b"/DecodeParms",
                ObjectHandle::dictionary(vec![
                    (b"/Predictor".to_vec(), ObjectHandle::integer(12)),
                    (b"/Colors".to_vec(), ObjectHandle::integer(1)),
                    (b"/BitsPerComponent".to_vec(), ObjectHandle::integer(8)),
                    (b"/Columns".to_vec(), ObjectHandle::integer(width as i64)),
                ]),
            )
            .expect("install predictor params");
        let encoded = crate::filters::encode_stream_data(&dictionary, &pixels)
            .expect("encode predictor-bearing source");
        dictionary
            .replace_key(b"/Length", ObjectHandle::integer(encoded.len() as i64))
            .expect("install source length");
        let image = ObjectHandle::stream(dictionary.clone(), Rc::new(encoded));
        let optimizer = match ImageOptimizer::prepare(
            image,
            ImageOptimizationOptions {
                min_width: 0,
                min_height: 0,
                min_area: 0,
                min_savings_bytes: 0,
                min_savings_percent: 0,
                ..ImageOptimizationOptions::default()
            },
        )
        .expect("prepare lossless resize source")
        {
            PrepareResult::Ready(optimizer) => optimizer,
            PrepareResult::Skip(_) => panic!("Flate resize source should be eligible"),
        };

        let resized = optimizer
            .resize_and_encode(ImageResizeTarget::flate(100, 50))
            .expect("resize Flate source")
            .expect("target is smaller than source");
        assert_eq!((resized.width, resized.height), (100, 50));
        assert_eq!(resized.encoding, ImageResizeEncoding::Flate);

        let resized_dictionary = image_dictionary(
            ObjectHandle::integer(resized.width.into()),
            ObjectHandle::integer(resized.height.into()),
        );
        resized_dictionary
            .replace_key(b"/Filter", ObjectHandle::name(b"FlateDecode".to_vec()))
            .expect("install resized Flate filter");
        resized_dictionary
            .replace_key(b"/DecodeParms", resized.decode_params.clone())
            .expect("install resized predictor params");
        assert_eq!(
            resized
                .decode_params
                .try_get_key(b"/Columns")
                .expect("columns")
                .as_integer(),
            Some(100)
        );
        let decoded = crate::filters::decode_stream_data(&resized_dictionary, &resized.encoded)
            .expect("decode resized lossless image");
        assert_eq!(decoded.len(), 100 * 50);
    }

    #[test]
    fn optimizer_keeps_a_source_when_jpeg_does_not_shrink_it() {
        let optimizer = prepared_gray_optimizer(1, 1, vec![128]);
        assert!(matches!(
            optimizer.evaluate().unwrap(),
            Some(Evaluation::NotSmaller)
        ));
    }

    #[test]
    fn optimizer_respects_configured_savings_gate() {
        let mut optimizer = prepared_gray_optimizer(200, 200, vec![128; 40_000]);
        assert!(matches!(
            optimizer.evaluate().unwrap(),
            Some(Evaluation::Smaller { .. })
        ));

        optimizer.min_savings_percent = 100;
        assert!(matches!(
            optimizer.evaluate().unwrap(),
            Some(Evaluation::BelowSavingsThreshold)
        ));
    }
}

/// Encoded replacement produced by the detached image optimizer.
#[derive(Debug, Clone)]
pub struct DetachedImageTransform {
    pub encoded: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub filter: Vec<u8>,
    pub decode_params: ObjectHandle,
    pub original_encoded_bytes: u64,
    pub original_pixels: u64,
    pub optimized_pixels: u64,
    pub resized: bool,
}

/// Optimize one detached Image XObject stream without a mutable `Pdf` document.
///
/// `target=None` performs the ordinary lossless-source -> JPEG optimization;
/// `Some(target)` performs the conservative caller-selected resize path.
/// The returned bytes have already passed the configured savings gates.
pub fn optimize_image_detached(
    image: ObjectHandle,
    options: ImageOptimizationOptions,
    target: Option<ImageResizeTarget>,
) -> Result<Option<DetachedImageTransform>> {
    if image.as_stream_dict().is_none() {
        return Ok(None);
    }
    if target.is_none() && !ImageOptimizer::preflight(&image)? {
        return Ok(None);
    }
    let optimizer = match ImageOptimizer::prepare(image.clone(), options)? {
        PrepareResult::Ready(optimizer) => optimizer,
        PrepareResult::Skip(_) => return Ok(None),
    };

    if let Some(target) = target {
        let dictionary = image
            .as_stream_dict()
            .ok_or_else(|| Error::Internal("detached image has no stream dictionary".to_owned()))?;
        let conservative = match target.encoding {
            ImageResizeEncoding::Jpeg => is_conservative_dct_resize_source(&dictionary)?,
            ImageResizeEncoding::Flate => is_conservative_flate_resize_source(&dictionary)?,
        };
        if !conservative {
            return Ok(None);
        }
        let Some(resized) = optimizer.resize_and_encode(target)? else {
            return Ok(None);
        };
        let original = resized.original_encoded_bytes;
        let optimized = resized.encoded.len() as u64;
        let savings = original.saturating_sub(optimized);
        if optimized >= original
            || savings < options.min_savings_bytes
            || u128::from(savings) * 100
                < u128::from(original) * u128::from(options.min_savings_percent)
        {
            return Ok(None);
        }
        return Ok(Some(DetachedImageTransform {
            encoded: resized.encoded,
            width: resized.width,
            height: resized.height,
            filter: match resized.encoding {
                ImageResizeEncoding::Jpeg => b"DCTDecode".to_vec(),
                ImageResizeEncoding::Flate => b"FlateDecode".to_vec(),
            },
            decode_params: resized.decode_params,
            original_encoded_bytes: original,
            original_pixels: u64::from(optimizer.width) * u64::from(optimizer.height),
            optimized_pixels: u64::from(resized.width) * u64::from(resized.height),
            resized: true,
        }));
    }

    let Some(evaluation) = optimizer.evaluate()? else {
        return Ok(None);
    };
    let Evaluation::Smaller {
        original_length,
        compressed_length: _,
    } = evaluation
    else {
        return Ok(None);
    };
    let mut encoded = Vec::new();
    {
        let mut sink = PlString::new("detached optimized jpeg", None, &mut encoded);
        let mut encoder = optimizer.encoder(&mut sink);
        let mut filtering_attempted = false;
        if !image.pipe_stream_data(
            &mut encoder,
            &mut filtering_attempted,
            0,
            DecodeLevel::Specialized,
            false,
            false,
        )? {
            return Ok(None);
        }
        encoder.finish()?;
    }
    Ok(Some(DetachedImageTransform {
        encoded,
        width: optimizer.width,
        height: optimizer.height,
        filter: b"DCTDecode".to_vec(),
        decode_params: ObjectHandle::null(),
        original_encoded_bytes: original_length,
        original_pixels: u64::from(optimizer.width) * u64::from(optimizer.height),
        optimized_pixels: u64::from(optimizer.width) * u64::from(optimizer.height),
        resized: false,
    }))
}
