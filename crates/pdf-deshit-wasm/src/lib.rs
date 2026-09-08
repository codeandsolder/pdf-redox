use pdf_deshit::{Config, HiddenTextPolicy, PrivacyLevel, analyze_pdf, optimize_pdf};
use serde::Serialize;
use wasm_bindgen::prelude::*;

#[derive(Serialize)]
struct WasmResult {
    pdf: Vec<u8>,
    report: pdf_deshit::OptimizationReport,
}

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

#[wasm_bindgen]
pub fn analyze(bytes: &[u8]) -> Result<JsValue, JsValue> {
    let report = analyze_pdf(bytes).map_err(|e| JsValue::from_str(&e.to_string()))?;
    serde_wasm_bindgen::to_value(&report).map_err(|e| JsValue::from_str(&e.to_string()))
}

fn config(profile: &str, privacy: &str) -> Config {
    let mut cfg = match profile {
        "perceptual" => Config::perceptual(),
        "print" => Config::print(),
        _ => Config::optimize_only(),
    };
    cfg.privacy.level = match privacy {
        "metadata" => PrivacyLevel::Metadata,
        "best-effort" => PrivacyLevel::BestEffort,
        _ => PrivacyLevel::None,
    };
    if cfg.privacy.level != PrivacyLevel::None {
        cfg.privacy.strip_jpeg_metadata = true;
    }
    cfg
}

fn run(bytes: &[u8], cfg: &Config) -> Result<JsValue, JsValue> {
    let (pdf, report) = optimize_pdf(bytes, cfg).map_err(|e| JsValue::from_str(&e.to_string()))?;
    serde_wasm_bindgen::to_value(&WasmResult { pdf, report })
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

#[wasm_bindgen]
pub fn optimize(bytes: &[u8], profile: &str, privacy: &str) -> Result<JsValue, JsValue> {
    run(bytes, &config(profile, privacy))
}

#[wasm_bindgen]
pub fn optimize_with_hidden_text(
    bytes: &[u8],
    profile: &str,
    privacy: &str,
    hidden_text_policy: JsValue,
) -> Result<JsValue, JsValue> {
    let mut cfg = config(profile, privacy);
    cfg.hidden_text = if hidden_text_policy.is_null() || hidden_text_policy.is_undefined() {
        HiddenTextPolicy::default()
    } else {
        serde_wasm_bindgen::from_value(hidden_text_policy)
            .map_err(|e| JsValue::from_str(&format!("invalid hidden-text policy: {e}")))?
    };
    run(bytes, &cfg)
}
