//! Platform shims. On wasm there is no clock without JavaScript, and timings
//! are diagnostics only, so `Instant` there measures nothing.
#[cfg(not(target_arch = "wasm32"))]
pub use std::time::Instant;

#[cfg(target_arch = "wasm32")]
#[derive(Clone, Copy)]
pub struct Instant;

#[cfg(target_arch = "wasm32")]
impl Instant {
    pub fn now() -> Instant {
        Instant
    }
    pub fn elapsed(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}

#[cfg(target_arch = "wasm32")]
impl std::ops::Sub for Instant {
    type Output = std::time::Duration;
    fn sub(self, _: Instant) -> std::time::Duration {
        std::time::Duration::ZERO
    }
}
