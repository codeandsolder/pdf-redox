use color_quant::NeuQuant;

const PALETTE_COLORS: usize = 256;
const QUANTIZER_SAMPLE_FACTOR: i32 = 10;
const MIN_PIXELS: usize = 500_000;
const PAPER_POOL_MIN_PIXELS: u64 = 100;
const MAX_CHANNEL_DELTA: u8 = 128;
const WHOLE_MSE_LIMIT: u64 = 6;
const FOREGROUND_MSE_LIMIT: u64 = 16;
const THRESHOLDS: [u8; 5] = [44, 32, 20, 12, 0];

#[derive(Debug)]
pub struct IndexedPaperImage {
    pub indices: Vec<u8>,
    pub palette: Vec<u8>,
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
