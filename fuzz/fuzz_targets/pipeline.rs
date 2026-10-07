#![no_main]

use libfuzzer_sys::fuzz_target;
use pdf_redox::{Config, OptimizationGoal, analyze_pdf, optimize_pdf};

fuzz_target!(|data: &[u8]| {
    let _ = analyze_pdf(data);

    let mut config = Config::optimize_only();
    config.optimization_goal = OptimizationGoal::Processing;
    config.raster_layout.enabled = true;
    config.raster_layout.bake_masks = true;
    let _ = optimize_pdf(data, &config);
});
