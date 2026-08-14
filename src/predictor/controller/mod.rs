use super::data::Entry;
use crate::als::Scale;
use std::time::{Duration, Instant};

pub mod adaptive;
pub mod manual;

const INITIAL_TIMEOUT: Duration = Duration::from_secs(15);
const PENDING_COOLDOWN: Duration = Duration::from_millis(1500);
const LUMA_SCALE: f64 = 20.0;
const MAX_DISTANCE: f64 = 5.0;

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

fn distance(scale: Scale, als: u64, luma: u8, entry: &Entry) -> f64 {
    let als_distance = scale.coordinate(als) - scale.coordinate(entry.als);
    let luma_distance = (luma as f64 - entry.luma as f64) / LUMA_SCALE;
    als_distance.hypot(luma_distance)
}

fn interpolate_raw(entries: &[Entry], scale: Scale, als: u64, luma: u8) -> Option<f64> {
    let points = entries
        .iter()
        .filter_map(|entry| {
            let distance = distance(scale, als, luma, entry);
            (distance <= MAX_DISTANCE).then_some((entry.brightness as f64, distance))
        })
        .collect::<Vec<_>>();
    if let Some((brightness, _)) = points.iter().find(|(_, distance)| *distance == 0.0) {
        return Some(*brightness);
    }
    let total_weight = points
        .iter()
        .map(|(_, distance)| 1.0 / distance.powi(2))
        .sum::<f64>();
    if total_weight == 0.0 {
        return None;
    }
    let prediction = points
        .iter()
        .map(|(brightness, distance)| brightness / distance.powi(2) / total_weight)
        .sum::<f64>();
    Some(prediction)
}

fn interpolate(entries: &[Entry], scale: Scale, als: u64, luma: u8) -> Option<u64> {
    interpolate_raw(entries, scale, als, luma).map(|prediction| prediction.round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolation_rounds_to_nearest_brightness() {
        let entries = vec![
            Entry::new(3, 0, 2),
            Entry::new(18, 0, 1),
            Entry::new(747, 0, 0),
        ];

        assert_eq!(interpolate(&entries, Scale::Lux, 5, 0), Some(2));
    }
}
