use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use smol::Timer;

#[derive(Default)]
pub struct Capturer {}

impl Capturer {
    pub async fn run(
        &mut self,
        _output_name: &str,
        mut controller: crate::predictor::Controller,
        active: Arc<AtomicBool>,
    ) {
        while active.load(Ordering::Relaxed) {
            controller.adjust(0).await;
            Timer::after(Duration::from_millis(200)).await;
        }
    }
}
