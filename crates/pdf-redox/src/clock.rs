#[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
pub(crate) use std::time::Instant;

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod browser {
    use std::time::Duration;
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(js_namespace = performance, js_name = now)]
        fn performance_now() -> f64;
    }

    #[derive(Clone, Copy)]
    pub(crate) struct Instant(f64);

    impl Instant {
        pub(crate) fn now() -> Self {
            Self(performance_now())
        }

        pub(crate) fn elapsed(self) -> Duration {
            Duration::try_from_secs_f64((performance_now() - self.0).max(0.0) / 1000.0)
                .unwrap_or_default()
        }
    }
}

#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
pub(crate) use browser::Instant;
