use color_quant::NeuQuant;
use std::collections::BTreeMap;

const PALETTE_COLORS: usize = 256;
const LOCAL_PALETTE_COLORS: usize = 255;
pub const LOCAL_LAYER_COUNTS: [usize; 3] = [2, 4, 8];
const QUANTIZER_SAMPLE_FACTOR: i32 = 10;
pub const LOCAL_QUANTIZER_SAMPLE_FACTORS: [i32; 2] = [20, 10];
const MIN_PIXELS: usize = 500_000;
const PAPER_POOL_MIN_PIXELS: u64 = 100;
const MAX_CHANNEL_DELTA: u8 = 128;
const WHOLE_MSE_LIMIT: u64 = 6;
const FOREGROUND_MSE_LIMIT: u64 = 16;
const THRESHOLDS: [u8; 5] = [44, 32, 20, 12, 0];
const WEIGHTED_FOREGROUND_THRESHOLD: u8 = 32;
const WEIGHTED_PAPER_STRIDE: usize = 4;
const RESIDUAL_ERROR_THRESHOLD: u8 = 16;
const RESIDUAL_PALETTE_COLORS: usize = 255;

#[derive(Debug)]
pub struct IndexedPaperImage {
    pub indices: Vec<u8>,
    pub palette: Vec<u8>,
}

#[derive(Debug)]
pub struct ResidualIndexedPaperImage {
    pub base_indices: Vec<u8>,
    pub base_palette: Vec<u8>,
    pub residual_indices: Vec<u8>,
    pub residual_palette: Vec<u8>,
}

#[derive(Debug)]
pub enum ForegroundIndexedPaperImage {
    Weighted(IndexedPaperImage),
    Residual(ResidualIndexedPaperImage),
}

#[derive(Debug)]
pub struct IndexedPaperLayer {
    pub row_start: usize,
    pub row_end: usize,
    pub indices: Vec<u8>,
    pub palette: Vec<u8>,
}

#[derive(Debug)]
pub struct LayeredIndexedPaperImage {
    pub layers: Vec<IndexedPaperLayer>,
}

#[derive(Debug, Clone, Copy)]
struct PaperStats {
    background: [u8; 3],
    pixels: usize,
}

pub fn quantize_rgb(rgb: &[u8]) -> Option<IndexedPaperImage> {
    let stats = paper_stats(rgb)?;
    let total_samples = u64::try_from(stats.pixels).ok()?.checked_mul(3)?;
    for threshold in THRESHOLDS {
        if flatten_sse(rgb, stats.background, threshold)? > total_samples * WHOLE_MSE_LIMIT {
            continue;
        }
        if let Some(candidate) = quantize_at_threshold(rgb, stats, threshold) {
            return Some(candidate);
        }
    }
    None
}

/// Quantize one full-grid sparse Indexed layer partition.
pub fn quantize_rgb_layer_count_with_sample_factor(
    rgb: &[u8],
    width: u32,
    height: u32,
    layer_count: usize,
    sample_factor: i32,
) -> Option<LayeredIndexedPaperImage> {
    let stats = paper_stats(rgb)?;
    let width = usize::try_from(width).ok()?;
    let height = usize::try_from(height).ok()?;
    if width.checked_mul(height)? != stats.pixels {
        return None;
    }
    quantize_local_layers(rgb, width, height, stats, layer_count, sample_factor)
}

fn quantize_local_layers(
    rgb: &[u8],
    width: usize,
    height: usize,
    stats: PaperStats,
    layer_count: usize,
    sample_factor: i32,
) -> Option<LayeredIndexedPaperImage> {
    let mut layers = Vec::with_capacity(layer_count);
    let mut whole_sse = 0_u64;
    let mut foreground_sse = 0_u64;
    let mut foreground_samples = 0_u64;
    let mut max_delta = 0_u8;
    let background_rgba = [
        stats.background[0],
        stats.background[1],
        stats.background[2],
        255,
    ];

    for layer_index in 0..layer_count {
        let row_start = height.checked_mul(layer_index)?.checked_div(layer_count)?;
        let row_end = height
            .checked_mul(layer_index.checked_add(1)?)?
            .checked_div(layer_count)?;
        if row_start >= row_end {
            return None;
        }
        let pixel_start = row_start.checked_mul(width)?;
        let pixel_end = row_end.checked_mul(width)?;
        let byte_start = pixel_start.checked_mul(3)?;
        let byte_end = pixel_end.checked_mul(3)?;
        let tile = rgb.get(byte_start..byte_end)?;
        let tile_pixels = tile.len().checked_div(3)?;
        let mut rgba = Vec::with_capacity(tile_pixels.checked_mul(4)?);
        for pixel in tile.as_chunks::<3>().0 {
            rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
        }
        let quantizer = NeuQuant::new(sample_factor, LOCAL_PALETTE_COLORS, &rgba);
        let mut palette = quantizer.color_map_rgb();
        if palette.len() != LOCAL_PALETTE_COLORS * 3 {
            return None;
        }
        let background_index = quantizer.index_of(&background_rgba);
        let background_start = background_index.checked_mul(3)?;
        palette
            .get_mut(background_start..background_start.checked_add(3)?)?
            .copy_from_slice(&stats.background);

        let mut indices = Vec::with_capacity(tile_pixels);
        for original in tile.as_chunks::<3>().0 {
            let index = if *original == stats.background {
                background_index
            } else {
                quantizer.index_of(&[original[0], original[1], original[2], 255])
            };
            let index_u8 = u8::try_from(index).ok()?;
            indices.push(index_u8);
            let palette_start = index.checked_mul(3)?;
            let reconstructed = palette.get(palette_start..palette_start.checked_add(3)?)?;
            let foreground = channel_distance(original, stats.background) > 0;
            for (&source, &sample) in original.iter().zip(reconstructed) {
                let delta = source.abs_diff(sample);
                max_delta = max_delta.max(delta);
                let square = u64::from(delta) * u64::from(delta);
                whole_sse = whole_sse.checked_add(square)?;
                if foreground {
                    foreground_sse = foreground_sse.checked_add(square)?;
                    foreground_samples = foreground_samples.checked_add(1)?;
                }
            }
        }
        layers.push(IndexedPaperLayer {
            row_start,
            row_end,
            indices,
            palette,
        });
    }

    let whole_samples = u64::try_from(stats.pixels).ok()?.checked_mul(3)?;
    if whole_sse > whole_samples.checked_mul(WHOLE_MSE_LIMIT)?
        || foreground_samples == 0
        || foreground_sse > foreground_samples.checked_mul(FOREGROUND_MSE_LIMIT)?
        || max_delta > MAX_CHANNEL_DELTA
    {
        return None;
    }
    Some(LayeredIndexedPaperImage { layers })
}

fn paper_stats(rgb: &[u8]) -> Option<PaperStats> {
    if !rgb.len().is_multiple_of(3) {
        return None;
    }
    let pixels = rgb.len() / 3;
    if pixels < MIN_PIXELS {
        return None;
    }

    let mut histograms = [[0_u64; 256]; 3];
    let mut paper_pool = 0_u64;
    let mut near_white = 0_u64;
    let mut neutral = 0_u64;
    let mut dark = 0_u64;

    for pixel in rgb.as_chunks::<3>().0 {
        let [r, g, b] = [pixel[0], pixel[1], pixel[2]];
        let sum = u16::from(r) + u16::from(g) + u16::from(b);
        let span = r.max(g).max(b) - r.min(g).min(b);

        near_white += u64::from(sum >= 220 * 3);
        neutral += u64::from(span <= 20);
        dark += u64::from(sum < 128 * 3);

        if sum >= 210 * 3 && span <= 18 {
            histograms[0][usize::from(r)] += 1;
            histograms[1][usize::from(g)] += 1;
            histograms[2][usize::from(b)] += 1;
            paper_pool += 1;
        }
    }

    let pixels_u64 = u64::try_from(pixels).ok()?;
    if paper_pool < PAPER_POOL_MIN_PIXELS
        || near_white * 100 < pixels_u64 * 80
        || neutral * 100 < pixels_u64 * 80
        || dark * 100 > pixels_u64 * 15
    {
        return None;
    }

    Some(PaperStats {
        background: [
            histogram_median(&histograms[0], paper_pool)?,
            histogram_median(&histograms[1], paper_pool)?,
            histogram_median(&histograms[2], paper_pool)?,
        ],
        pixels,
    })
}

fn histogram_median(histogram: &[u64; 256], count: u64) -> Option<u8> {
    if count == 0 {
        return None;
    }
    let target = count.div_ceil(2);
    let mut accumulated = 0_u64;
    for (index, &bucket) in histogram.iter().enumerate() {
        accumulated += bucket;
        if accumulated >= target {
            return u8::try_from(index).ok();
        }
    }
    None
}

fn channel_distance(pixel: &[u8], background: [u8; 3]) -> u8 {
    pixel
        .iter()
        .copied()
        .zip(background)
        .map(|(sample, paper)| sample.abs_diff(paper))
        .max()
        .unwrap_or(0)
}

fn flatten_sse(rgb: &[u8], background: [u8; 3], threshold: u8) -> Option<u64> {
    let mut sse = 0_u64;
    for pixel in rgb.as_chunks::<3>().0 {
        if channel_distance(pixel, background) <= threshold {
            for (&sample, paper) in pixel.iter().zip(background) {
                let delta = i16::from(sample) - i16::from(paper);
                let square = i32::from(delta) * i32::from(delta);
                sse = sse.checked_add(u64::try_from(square).ok()?)?;
            }
        }
    }
    Some(sse)
}

fn weighted_training_rgba(rgb: &[u8], background: [u8; 3]) -> Option<Vec<u8>> {
    let mut training = Vec::new();
    let mut paper_index = 0_usize;
    for pixel in rgb.as_chunks::<3>().0 {
        let foreground = channel_distance(pixel, background) > WEIGHTED_FOREGROUND_THRESHOLD;
        if foreground || paper_index.is_multiple_of(WEIGHTED_PAPER_STRIDE) {
            training.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
        }
        if !foreground {
            paper_index = paper_index.checked_add(1)?;
        }
    }
    Some(training)
}

fn quantize_foreground_weighted_candidate(
    rgb: &[u8],
    stats: PaperStats,
) -> Option<IndexedPaperImage> {
    let training = weighted_training_rgba(rgb, stats.background)?;
    if training.len() < PALETTE_COLORS.checked_mul(4)? {
        return None;
    }

    let quantizer = NeuQuant::new(QUANTIZER_SAMPLE_FACTOR, PALETTE_COLORS, &training);
    let mut palette = quantizer.color_map_rgb();
    if palette.len() != PALETTE_COLORS * 3 {
        return None;
    }

    let background_rgba = [
        stats.background[0],
        stats.background[1],
        stats.background[2],
        255,
    ];
    let background_index = quantizer.index_of(&background_rgba);
    let background_start = background_index.checked_mul(3)?;
    palette
        .get_mut(background_start..background_start.checked_add(3)?)?
        .copy_from_slice(&stats.background);

    let mut indices = Vec::with_capacity(stats.pixels);
    for original in rgb.as_chunks::<3>().0 {
        let index = if *original == stats.background {
            background_index
        } else {
            quantizer.index_of(&[original[0], original[1], original[2], 255])
        };
        indices.push(u8::try_from(index).ok()?);
    }
    Some(IndexedPaperImage { indices, palette })
}

fn indexed_quality_ok(
    rgb: &[u8],
    stats: PaperStats,
    candidate: &IndexedPaperImage,
    foreground_threshold: u8,
) -> Option<bool> {
    if candidate.indices.len() != stats.pixels || candidate.palette.len() != PALETTE_COLORS * 3 {
        return None;
    }
    let mut whole_sse = 0_u64;
    let mut foreground_sse = 0_u64;
    let mut foreground_samples = 0_u64;
    let mut max_delta = 0_u8;
    for (original, &index) in rgb.as_chunks::<3>().0.iter().zip(&candidate.indices) {
        let palette_start = usize::from(index).checked_mul(3)?;
        let reconstructed = candidate
            .palette
            .get(palette_start..palette_start.checked_add(3)?)?;
        let foreground = channel_distance(original, stats.background) > foreground_threshold;
        for (&source, &sample) in original.iter().zip(reconstructed) {
            let delta = source.abs_diff(sample);
            max_delta = max_delta.max(delta);
            let square = u64::from(delta) * u64::from(delta);
            whole_sse = whole_sse.checked_add(square)?;
            if foreground {
                foreground_sse = foreground_sse.checked_add(square)?;
                foreground_samples = foreground_samples.checked_add(1)?;
            }
        }
    }
    let whole_samples = u64::try_from(stats.pixels).ok()?.checked_mul(3)?;
    Some(
        whole_sse <= whole_samples.checked_mul(WHOLE_MSE_LIMIT)?
            && foreground_samples > 0
            && foreground_sse <= foreground_samples.checked_mul(FOREGROUND_MSE_LIMIT)?
            && max_delta <= MAX_CHANNEL_DELTA,
    )
}

fn residual_palette(colors: &[[u8; 3]]) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut exact = BTreeMap::<[u8; 3], u8>::new();
    let mut exact_possible = true;
    for &color in colors {
        if exact.contains_key(&color) {
            continue;
        }
        if exact.len() >= RESIDUAL_PALETTE_COLORS {
            exact_possible = false;
            break;
        }
        exact.insert(color, u8::try_from(exact.len()).ok()?);
    }
    if exact_possible {
        let mut palette = vec![0_u8; exact.len().checked_mul(3)?];
        let mut indices = Vec::with_capacity(colors.len());
        for &color in colors {
            let index = *exact.get(&color)?;
            let start = usize::from(index).checked_mul(3)?;
            palette
                .get_mut(start..start.checked_add(3)?)?
                .copy_from_slice(&color);
            indices.push(index);
        }
        return Some((indices, palette));
    }

    let mut rgba = Vec::with_capacity(colors.len().checked_mul(4)?);
    for &color in colors {
        rgba.extend_from_slice(&[color[0], color[1], color[2], 255]);
    }
    let quantizer = NeuQuant::new(QUANTIZER_SAMPLE_FACTOR, RESIDUAL_PALETTE_COLORS, &rgba);
    let palette = quantizer.color_map_rgb();
    if palette.len() != RESIDUAL_PALETTE_COLORS * 3 {
        return None;
    }
    let indices = colors
        .iter()
        .map(|color| u8::try_from(quantizer.index_of(&[color[0], color[1], color[2], 255])).ok())
        .collect::<Option<Vec<_>>>()?;
    Some((indices, palette))
}

fn quantize_residual_from_base(
    rgb: &[u8],
    stats: PaperStats,
    base: IndexedPaperImage,
) -> Option<ResidualIndexedPaperImage> {
    let mut selected_positions = Vec::new();
    let mut selected_colors = Vec::new();
    for (position, (original, &index)) in
        rgb.as_chunks::<3>().0.iter().zip(&base.indices).enumerate()
    {
        let start = usize::from(index).checked_mul(3)?;
        let reconstructed = base.palette.get(start..start.checked_add(3)?)?;
        if channel_distance(
            original,
            [reconstructed[0], reconstructed[1], reconstructed[2]],
        ) > RESIDUAL_ERROR_THRESHOLD
        {
            selected_positions.push(position);
            selected_colors.push([original[0], original[1], original[2]]);
        }
    }
    if selected_colors.is_empty() {
        return None;
    }
    let (selected_indices, residual_palette) = residual_palette(&selected_colors)?;
    let mut residual_indices = vec![0_u8; stats.pixels];
    for (&position, &index) in selected_positions.iter().zip(&selected_indices) {
        *residual_indices.get_mut(position)? = index.checked_add(1)?;
    }

    let mut whole_sse = 0_u64;
    let mut foreground_sse = 0_u64;
    let mut foreground_samples = 0_u64;
    let mut max_delta = 0_u8;
    for (position, original) in rgb.as_chunks::<3>().0.iter().enumerate() {
        let residual = *residual_indices.get(position)?;
        let reconstructed = if residual == 0 {
            let index = usize::from(*base.indices.get(position)?);
            let start = index.checked_mul(3)?;
            base.palette.get(start..start.checked_add(3)?)?
        } else {
            let index = usize::from(residual.checked_sub(1)?);
            let start = index.checked_mul(3)?;
            residual_palette.get(start..start.checked_add(3)?)?
        };
        let foreground = channel_distance(original, stats.background) > 0;
        for (&source, &sample) in original.iter().zip(reconstructed) {
            let delta = source.abs_diff(sample);
            max_delta = max_delta.max(delta);
            let square = u64::from(delta) * u64::from(delta);
            whole_sse = whole_sse.checked_add(square)?;
            if foreground {
                foreground_sse = foreground_sse.checked_add(square)?;
                foreground_samples = foreground_samples.checked_add(1)?;
            }
        }
    }
    let whole_samples = u64::try_from(stats.pixels).ok()?.checked_mul(3)?;
    if whole_sse > whole_samples.checked_mul(WHOLE_MSE_LIMIT)?
        || foreground_samples == 0
        || foreground_sse > foreground_samples.checked_mul(FOREGROUND_MSE_LIMIT)?
        || max_delta > MAX_CHANNEL_DELTA
    {
        return None;
    }

    Some(ResidualIndexedPaperImage {
        base_indices: base.indices,
        base_palette: base.palette,
        residual_indices,
        residual_palette,
    })
}

pub fn quantize_rgb_foreground_weighted_or_residual(
    rgb: &[u8],
) -> Option<ForegroundIndexedPaperImage> {
    let stats = paper_stats(rgb)?;
    let base = quantize_foreground_weighted_candidate(rgb, stats)?;
    if indexed_quality_ok(rgb, stats, &base, 0)? {
        return Some(ForegroundIndexedPaperImage::Weighted(base));
    }
    quantize_residual_from_base(rgb, stats, base).map(ForegroundIndexedPaperImage::Residual)
}

fn quantize_at_threshold(
    rgb: &[u8],
    stats: PaperStats,
    threshold: u8,
) -> Option<IndexedPaperImage> {
    let rgba_len = stats.pixels.checked_mul(4)?;
    let mut rgba = Vec::with_capacity(rgba_len);
    for pixel in rgb.as_chunks::<3>().0 {
        let color = if channel_distance(pixel, stats.background) <= threshold {
            stats.background
        } else {
            [pixel[0], pixel[1], pixel[2]]
        };
        rgba.extend_from_slice(&[color[0], color[1], color[2], 255]);
    }

    let quantizer = NeuQuant::new(QUANTIZER_SAMPLE_FACTOR, PALETTE_COLORS, &rgba);
    let mut palette = quantizer.color_map_rgb();
    if palette.len() != PALETTE_COLORS * 3 {
        return None;
    }

    let background_rgba = [
        stats.background[0],
        stats.background[1],
        stats.background[2],
        255,
    ];
    let background_index = quantizer.index_of(&background_rgba);
    let background_start = background_index.checked_mul(3)?;
    palette
        .get_mut(background_start..background_start + 3)?
        .copy_from_slice(&stats.background);

    let mut indices = Vec::with_capacity(stats.pixels);
    let mut whole_sse = 0_u64;
    let mut foreground_sse = 0_u64;
    let mut foreground_samples = 0_u64;
    let mut max_delta = 0_u8;

    for (original, flattened) in rgb.as_chunks::<3>().0.iter().zip(rgba.as_chunks::<4>().0) {
        let index = if flattened[..3] == stats.background {
            background_index
        } else {
            quantizer.index_of(flattened)
        };
        let index_u8 = u8::try_from(index).ok()?;
        indices.push(index_u8);

        let palette_start = index.checked_mul(3)?;
        let reconstructed = palette.get(palette_start..palette_start + 3)?;
        let foreground = channel_distance(original, stats.background) > threshold;
        for (&source, &sample) in original.iter().zip(reconstructed) {
            let delta = source.abs_diff(sample);
            max_delta = max_delta.max(delta);
            let square = u64::from(delta) * u64::from(delta);
            whole_sse = whole_sse.checked_add(square)?;
            if foreground {
                foreground_sse = foreground_sse.checked_add(square)?;
                foreground_samples += 1;
            }
        }
    }

    let whole_samples = u64::try_from(stats.pixels).ok()?.checked_mul(3)?;
    if whole_sse > whole_samples * WHOLE_MSE_LIMIT
        || foreground_samples == 0
        || foreground_sse > foreground_samples * FOREGROUND_MSE_LIMIT
        || max_delta > MAX_CHANNEL_DELTA
    {
        return None;
    }

    Some(IndexedPaperImage { indices, palette })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paper_scan(width: usize, height: usize) -> Vec<u8> {
        let mut rgb = vec![255_u8; width * height * 3];
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
    fn residual_palette_preserves_small_color_set_exactly() {
        let colors = [[10, 20, 30], [200, 80, 40], [10, 20, 30], [0, 255, 120]];
        let candidate = residual_palette(&colors);
        assert!(candidate.is_some());
        let Some((indices, palette)) = candidate else {
            return;
        };
        assert_eq!(indices.len(), colors.len());
        for (&index, expected) in indices.iter().zip(colors) {
            let start = usize::from(index) * 3;
            assert_eq!(&palette[start..start + 3], expected.as_slice());
        }
    }

    #[test]
    fn weighted_training_keeps_foreground_and_subsamples_paper() {
        let background = [240_u8, 240, 240];
        let rgb = [
            240, 240, 240, // paper 0: kept
            241, 240, 240, // paper 1
            10, 100, 200, // foreground: always kept
            242, 240, 240, // paper 2
            243, 240, 240, // paper 3
            244, 240, 240, // paper 4: kept
            200, 20, 20, // foreground: always kept
            245, 240, 240, // paper 5
            246, 240, 240, // paper 6
        ];
        assert_eq!(
            weighted_training_rgba(&rgb, background),
            Some(vec![
                240, 240, 240, 255, 10, 100, 200, 255, 244, 240, 240, 255, 200, 20, 20, 255,
            ])
        );
    }

    #[test]
    fn paper_scan_quantizes_to_indexed_palette() {
        let rgb = paper_scan(800, 700);
        let candidate = quantize_rgb(&rgb);
        assert!(candidate.is_some());
        if let Some(candidate) = candidate {
            assert_eq!(candidate.indices.len(), 800 * 700);
            assert_eq!(candidate.palette.len(), PALETTE_COLORS * 3);
        }
    }

    #[test]
    fn paper_scan_quantizes_into_full_grid_local_palette_layers() {
        let width = 800_u32;
        let height = 700_u32;
        let rgb = paper_scan(800, 700);
        let candidate = LOCAL_LAYER_COUNTS.iter().copied().find_map(|layer_count| {
            quantize_rgb_layer_count_with_sample_factor(
                &rgb,
                width,
                height,
                layer_count,
                QUANTIZER_SAMPLE_FACTOR,
            )
        });
        assert!(candidate.is_some());
        if let Some(candidate) = candidate {
            assert!(matches!(candidate.layers.len(), 2 | 4 | 8));
            let mut next_row = 0_usize;
            for layer in candidate.layers {
                assert_eq!(layer.row_start, next_row);
                assert!(layer.row_end > layer.row_start);
                assert_eq!(layer.palette.len(), LOCAL_PALETTE_COLORS * 3);
                assert_eq!(layer.indices.len(), (layer.row_end - layer.row_start) * 800);
                assert!(
                    layer
                        .indices
                        .iter()
                        .all(|&index| usize::from(index) < LOCAL_PALETTE_COLORS)
                );
                next_row = layer.row_end;
            }
            assert_eq!(next_row, 700);
        }
    }

    #[test]
    fn colorful_non_paper_image_is_rejected() {
        let width = 800_usize;
        let height = 700_usize;
        let mut rgb = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            for x in 0..width {
                rgb.push(u8::try_from(x % 256).unwrap_or_default());
                rgb.push(u8::try_from(y % 256).unwrap_or_default());
                rgb.push(u8::try_from((x + y) % 256).unwrap_or_default());
            }
        }
        assert!(quantize_rgb(&rgb).is_none());
    }
}
