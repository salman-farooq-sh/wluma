use smol::channel::Sender;
use smol::Timer;

use super::Als;

const FILTER_TIME_CONSTANT_SECONDS: f64 = 0.5;

pub struct Controller {
    als: Als,
    value_txs: Vec<Sender<u64>>,
    filtered: Option<f64>,
}

impl Controller {
    pub fn new(als: Als, value_txs: Vec<Sender<u64>>) -> Self {
        Self {
            als,
            value_txs,
            filtered: None,
        }
    }

    pub async fn run(&mut self) {
        loop {
            self.step().await;
        }
    }

    async fn step(&mut self) {
        match self.als.get().await {
            Ok(value) => {
                let previous = self.filtered.unwrap_or(value as f64);
                let seconds = self.als.poll_interval().as_secs_f64();
                let weight = 1.0 - (-seconds / FILTER_TIME_CONSTANT_SECONDS).exp();
                let filtered = previous + weight * (value as f64 - previous);
                self.filtered = Some(filtered);
                let value = filtered.round() as u64;
                futures_util::future::try_join_all(
                    self.value_txs.iter().map(|channel| channel.send(value)),
                )
                .await
                .expect("Unable to send new ALS value, channel is dead");
            }
            Err(error) => log::error!("Unable to get ALS value: {error:?}"),
        }

        Timer::after(self.als.poll_interval()).await;
    }
}
