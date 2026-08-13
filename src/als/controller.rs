use smol::channel::Sender;
use smol::Timer;
use std::time::{Duration, Instant};

use super::{Als, Reading, Scale};

const FILTER_TIME_CONSTANT_SECONDS: f64 = 0.5;
const STABILIZATION_DURATION: Duration = Duration::from_secs(5);
const LUX_STABILIZATION_COORDINATE_TOLERANCE: f64 = 0.1;
const LUX_STABILIZATION_ABSOLUTE_TOLERANCE: u64 = 3;
const LINEAR_STABILIZATION_TOLERANCE: u64 = 10;

struct Candidate {
    since: Instant,
    min: u64,
    max: u64,
}

impl Candidate {
    fn new(value: u64, now: Instant) -> Self {
        Self {
            since: now,
            min: value,
            max: value,
        }
    }

    fn update(&mut self, scale: Scale, value: u64, now: Instant) -> bool {
        let min = self.min.min(value);
        let max = self.max.max(value);
        if !near(scale, min, max) {
            *self = Self::new(value, now);
            false
        } else {
            self.min = min;
            self.max = max;
            now.duration_since(self.since) >= STABILIZATION_DURATION
        }
    }
}

struct Stabilizer {
    scale: Scale,
    filtered: Option<f64>,
    candidate: Option<Candidate>,
}

impl Stabilizer {
    fn new(scale: Scale) -> Self {
        Self {
            scale,
            filtered: None,
            candidate: None,
        }
    }

    fn observe(&mut self, value: u64, interval: Duration, now: Instant) -> Option<Reading> {
        let Some(filtered) = self.filtered else {
            let stable = match self.candidate.as_mut() {
                Some(candidate) => candidate.update(self.scale, value, now),
                None => {
                    self.candidate = Some(Candidate::new(value, now));
                    false
                }
            };
            if !stable {
                return None;
            }
            self.filtered = Some(value as f64);
            self.candidate = None;
            return Some(self.reading(true));
        };

        if near(self.scale, filtered.round() as u64, value) {
            self.candidate = None;
            let weight = 1.0 - (-interval.as_secs_f64() / FILTER_TIME_CONSTANT_SECONDS).exp();
            self.filtered = Some(filtered + weight * (value as f64 - filtered));
            return Some(self.reading(true));
        }

        let stable = match self.candidate.as_mut() {
            Some(candidate) => candidate.update(self.scale, value, now),
            None => {
                self.candidate = Some(Candidate::new(value, now));
                false
            }
        };
        if stable {
            self.filtered = Some(value as f64);
            self.candidate = None;
            Some(self.reading(true))
        } else {
            Some(self.reading(false))
        }
    }

    fn reading(&self, stable: bool) -> Reading {
        Reading {
            value: self
                .filtered
                .expect("Stable ALS value must be known")
                .round() as u64,
            stable,
        }
    }
}

fn near(scale: Scale, left: u64, right: u64) -> bool {
    match scale {
        Scale::Lux => {
            left.abs_diff(right) <= LUX_STABILIZATION_ABSOLUTE_TOLERANCE
                || (scale.coordinate(left) - scale.coordinate(right)).abs()
                    <= LUX_STABILIZATION_COORDINATE_TOLERANCE
        }
        Scale::Linear => left.abs_diff(right) <= LINEAR_STABILIZATION_TOLERANCE,
    }
}

pub struct Controller {
    als: Als,
    value_txs: Vec<Sender<Reading>>,
    stabilizer: Stabilizer,
}

impl Controller {
    pub fn new(als: Als, value_txs: Vec<Sender<Reading>>) -> Self {
        let stabilizer = Stabilizer::new(als.scale());
        Self {
            als,
            value_txs,
            stabilizer,
        }
    }

    pub async fn run(&mut self) {
        loop {
            self.step().await;
        }
    }

    async fn step(&mut self) {
        match self.als.get().await {
            Ok(Some(value)) => {
                if let Some(reading) =
                    self.stabilizer
                        .observe(value, self.als.poll_interval(), Instant::now())
                {
                    futures_util::future::try_join_all(
                        self.value_txs.iter().map(|channel| channel.send(reading)),
                    )
                    .await
                    .expect("Unable to send new ALS value, channel is dead");
                }
            }
            Ok(None) => {}
            Err(error) => log::error!("Unable to get ALS value: {error:?}"),
        }

        Timer::after(self.als.poll_interval()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stability_uses_source_scale() {
        assert!(near(Scale::Lux, 100, 125));
        assert!(!near(Scale::Lux, 100, 130));
        assert!(near(Scale::Lux, 0, 3));
        assert!(near(Scale::Linear, 40, 50));
        assert!(!near(Scale::Linear, 40, 51));
    }

    #[test]
    fn waits_for_initial_stability() {
        let start = Instant::now();
        let mut stabilizer = Stabilizer::new(Scale::Linear);
        assert_eq!(None, stabilizer.observe(100, Duration::ZERO, start));
        assert_eq!(
            Some(Reading {
                value: 105,
                stable: true,
            }),
            stabilizer.observe(105, Duration::ZERO, start + STABILIZATION_DURATION)
        );
    }

    #[test]
    fn holds_the_accepted_value_while_confirming_a_jump() {
        let start = Instant::now();
        let mut stabilizer = Stabilizer::new(Scale::Linear);
        stabilizer.filtered = Some(10.0);
        assert_eq!(
            Some(Reading {
                value: 10,
                stable: false,
            }),
            stabilizer.observe(100, Duration::ZERO, start)
        );
        assert_eq!(
            Some(Reading {
                value: 100,
                stable: true,
            }),
            stabilizer.observe(100, Duration::ZERO, start + STABILIZATION_DURATION)
        );
    }

    #[test]
    fn returning_to_the_accepted_value_ends_confirmation() {
        let start = Instant::now();
        let mut stabilizer = Stabilizer::new(Scale::Linear);
        stabilizer.filtered = Some(10.0);
        stabilizer.observe(100, Duration::ZERO, start);
        assert_eq!(
            Some(Reading {
                value: 10,
                stable: true,
            }),
            stabilizer.observe(10, Duration::ZERO, start + Duration::from_secs(1))
        );
    }
}
