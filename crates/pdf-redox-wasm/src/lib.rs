use js_sys::{JSON, Object, Reflect, Uint8Array};
use pdf_redox::{
    Config, HiddenTextPolicy, PdfAnalysis, PrivacyLevel, analyze_pdf, optimize_pdf,
    optimize_pdf_with_analysis,
};
use serde::Serialize;
use std::cell::RefCell;
use wasm_bindgen::prelude::*;

thread_local! {
    static LAST_ANALYSIS: RefCell<Option<PdfAnalysis>> = const { RefCell::new(None) };
}

fn json_to_js<T: Serialize>(value: &T) -> Result<JsValue, JsValue> {
    let json = serde_json::to_string(value).map_err(|e| JsValue::from_str(&e.to_string()))?;
    JSON::parse(&json)
}

fn hidden_text_from_js(value: &JsValue) -> Result<HiddenTextPolicy, JsValue> {
    let json = JSON::stringify(value)?
        .as_string()
        .ok_or_else(|| JsValue::from_str("hidden-text policy is not JSON serializable"))?;
    serde_json::from_str(&json)
        .map_err(|e| JsValue::from_str(&format!("invalid hidden-text policy: {e}")))
}

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// Analyze a PDF and serialize the report for JavaScript.
///
/// # Errors
///
/// Returns a JavaScript error value when PDF analysis or report serialization fails.
#[wasm_bindgen]
pub fn analyze(bytes: &[u8]) -> Result<JsValue, JsValue> {
    let report = analyze_pdf(bytes).map_err(|e| JsValue::from_str(&e.to_string()))?;
    let value = json_to_js(&report)?;
    LAST_ANALYSIS.with(|slot| *slot.borrow_mut() = Some(report));
    Ok(value)
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
    let optimized = LAST_ANALYSIS.with(|slot| {
        let cached = slot.borrow();
        cached
            .as_ref()
            .filter(|analysis| analysis.matches_input(bytes))
            .map_or_else(
                || optimize_pdf(bytes, cfg),
                |analysis| optimize_pdf_with_analysis(bytes, cfg, analysis),
            )
    });
    let (pdf, report) = optimized.map_err(|e| JsValue::from_str(&e.to_string()))?;
    LAST_ANALYSIS.with(|slot| *slot.borrow_mut() = Some(report.before.clone()));
    let result = Object::new();
    let pdf = Uint8Array::from(pdf.as_slice());
    if !Reflect::set(&result, &JsValue::from_str("pdf"), &pdf)?
        || !Reflect::set(&result, &JsValue::from_str("report"), &json_to_js(&report)?)?
    {
        return Err(JsValue::from_str("cannot construct optimization result"));
    }
    Ok(result.into())
}

/// Optimize a PDF using the named profile and privacy mode.
///
/// # Errors
///
/// Returns a JavaScript error value when optimization or result serialization fails.
#[wasm_bindgen]
pub fn optimize(bytes: &[u8], profile: &str, privacy: &str) -> Result<JsValue, JsValue> {
    run(bytes, &config(profile, privacy))
}

/// Optimize a PDF with an explicit hidden-text policy supplied by JavaScript.
///
/// # Errors
///
/// Returns a JavaScript error value when the policy is invalid, optimization fails, or serialization fails.
#[wasm_bindgen]
pub fn optimize_with_hidden_text(
    bytes: &[u8],
    profile: &str,
    privacy: &str,
    hidden_text_policy: &JsValue,
) -> Result<JsValue, JsValue> {
    let mut cfg = config(profile, privacy);
    cfg.hidden_text = if hidden_text_policy.is_null() || hidden_text_policy.is_undefined() {
        HiddenTextPolicy::default()
    } else {
        hidden_text_from_js(hidden_text_policy)?
    };
    run(bytes, &cfg)
}
