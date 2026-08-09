use super::data::Entry;
use itertools::Itertools;
use std::time::{Duration, Instant};

pub mod adaptive;
pub mod manual;

const INITIAL_TIMEOUT: Duration = Duration::from_secs(15);
const PENDING_COOLDOWN: Duration = Duration::from_millis(1500);
const NEXT_ALS_COOLDOWN: Duration = Duration::from_millis(1500);

#[derive(Default)]
struct Cooldown {
    until: Option<Instant>,
}

impl Cooldown {
    fn reset(&mut self, duration: Duration) {
        self.until = Some(Instant::now() + duration);
    }

    fn is_active(&self) -> bool {
        self.until.is_some_and(|until| Instant::now() < until)
    }

    fn is_finished(&self) -> bool {
        self.until.is_some_and(|until| Instant::now() >= until)
    }

    fn clear(&mut self) {
        self.until = None;
    }

    #[cfg(test)]
    fn finish(&mut self) {
        self.until = Some(Instant::now());
    }
}

#[allow(clippy::large_enum_variant)]
pub enum Controller {
    Adaptive(adaptive::Controller),
    Manual(manual::Controller),
}

impl Controller {
    pub async fn adjust(&mut self, luma: u8) {
        match self {
            Self::Adaptive(c) => c.adjust(luma).await,
            Self::Manual(c) => c.adjust(luma).await,
        }
    }
}

fn interpolate(entries: &[Entry], lux: &str, luma: u8) -> Option<u64> {
    let points = entries
        .iter()
        .filter(|e| e.lux == lux)
        .map(|entry| {
            let distance = (luma as f64 - entry.luma as f64).abs();
            (entry.brightness as f64, distance)
        })
        .collect_vec();

    if points.is_empty() {
        return None;
    }

    let points = points
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let other_distances: f64 = points[0..i]
                .iter()
                .chain(&points[i + 1..])
                .map(|p| p.1)
                .product();
            (p.0, p.1, other_distances)
        })
        .collect_vec();

    let distance_denominator: f64 = points
        .iter()
        .map(|p| p.1)
        .combinations(points.len() - 1)
        .map(|c| c.iter().product::<f64>())
        .sum();

    let prediction = points
        .iter()
        .map(|p| p.0 * p.2 / distance_denominator)
        .sum::<f64>() as u64;

    Some(prediction)
}
