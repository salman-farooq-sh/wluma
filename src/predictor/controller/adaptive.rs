use smol::channel::{Receiver, Sender};

use super::{distance, Cooldown, INITIAL_TIMEOUT, PENDING_COOLDOWN};
use crate::{
    als::{Reading, Scale},
    channel_ext::ReceiverExt,
    predictor::{
        data::{Data, Entry},
        AlsDirection,
    },
};
use std::collections::HashMap;

const REPLACEMENT_DISTANCE: f64 = 0.25;

pub struct Controller {
    prediction_tx: Sender<u64>,
    user_rx: Receiver<u64>,
    als_rx: Receiver<Reading>,
    pending_cooldown: Cooldown,
    pending: Option<Entry>,
    data: Data,
    stateful: bool,
    initial_brightness: Option<u64>,
    last_als: Option<Reading>,
    output_name: String,
    scale: Scale,
    als_direction: AlsDirection,
}

impl Controller {
    pub fn new(
        prediction_tx: Sender<u64>,
        user_rx: Receiver<u64>,
        als_rx: Receiver<Reading>,
        stateful: bool,
        output_name: &str,
        scale: Scale,
        legacy_thresholds: &HashMap<u64, String>,
    ) -> Self {
        let data = if stateful {
            Data::load(output_name, legacy_thresholds, scale)
        } else {
            Data::new(output_name)
        };

        Self {
            prediction_tx,
            user_rx,
            als_rx,
            pending_cooldown: Cooldown::default(),
            pending: None,
            data,
            stateful,
            initial_brightness: None,
            last_als: None,
            output_name: output_name.to_string(),
            scale,
            als_direction: AlsDirection::Increasing,
        }
    }

    pub fn with_als_direction(mut self, als_direction: AlsDirection) -> Self {
        self.als_direction = als_direction;
        self
    }

    pub async fn adjust(&mut self, luma: u8) {
        if self.last_als.is_none() {
            // ALS controller is expected to send the initial value on this channel asap
            self.last_als = Some(
                self.als_rx
                    .recv_or_panic_after_timeout(INITIAL_TIMEOUT)
                    .await
                    .expect("als_rx closed unexpectedly"),
            );

            // Brightness controller is expected to send the initial value on this channel asap
            let initial_brightness = self
                .user_rx
                .recv_or_panic_after_timeout(INITIAL_TIMEOUT)
                .await
                .expect("user_rx closed unexpectedly");

            // If there are no learned entries yet, we will use this as the first data point,
            // assuming that user is happy with the current brightness settings
            if self.data.entries.is_empty() {
                self.initial_brightness = Some(initial_brightness);
            };
        }

        if let Some(als) = self
            .als_rx
            .recv_maybe_last()
            .await
            .expect("als_rx closed unexpectedly")
        {
            self.last_als = Some(als);
        }

        let reading = self.last_als.expect("ALS value must be known");
        self.process_reading(reading, luma).await;
    }

    #[cfg(test)]
    async fn process(&mut self, als: u64, luma: u8) {
        self.process_reading(
            Reading {
                value: als,
                stable: true,
            },
            luma,
        )
        .await;
    }

    async fn process_reading(&mut self, reading: Reading, luma: u8) {
        let initial_brightness = self.initial_brightness.take();
        let user_changed_brightness = self
            .user_rx
            .recv_maybe_last()
            .await
            .expect("user_rx closed unexpectedly")
            .or(initial_brightness);

        if let Some(brightness) = user_changed_brightness {
            self.pending = match &self.pending {
                None => Some(Entry::new(reading.value, luma, brightness)),
                Some(Entry { als, .. }) => Some(Entry::new(
                    if reading.stable { reading.value } else { *als },
                    luma,
                    brightness,
                )),
            };
            self.pending_cooldown.reset(PENDING_COOLDOWN);
            return;
        }

        if !reading.stable {
            return;
        }

        if let Some(pending) = self.pending.as_mut() {
            pending.als = reading.value;
            if !self.pending_cooldown.is_active() {
                self.pending_cooldown.clear();
                self.learn();
            }
        } else if !self.pending_cooldown.is_active() {
            self.pending_cooldown.clear();
            self.predict(reading.value, luma).await;
        }
    }

    fn learn(&mut self) {
        let pending = self.pending.take().expect("No pending entry to learn");
        log::debug!("[{}] Learning {pending:?}", self.output_name);

        self.data.entries.retain(|entry| {
            let nearby =
                distance(self.scale, pending.als, pending.luma, entry) <= REPLACEMENT_DISTANCE;
            let pending_should_be_brighter = match self.als_direction {
                AlsDirection::Increasing => entry.als <= pending.als,
                AlsDirection::Decreasing => entry.als >= pending.als,
            } && entry.luma >= pending.luma;
            let pending_should_be_dimmer = match self.als_direction {
                AlsDirection::Increasing => entry.als >= pending.als,
                AlsDirection::Decreasing => entry.als <= pending.als,
            } && entry.luma <= pending.luma;
            let conflict = (pending_should_be_brighter && entry.brightness > pending.brightness)
                || (pending_should_be_dimmer && entry.brightness < pending.brightness);
            !nearby && !conflict
        });

        self.data.entries.push(pending);

        self.data
            .entries
            .sort_unstable_by(|x, y| x.als.cmp(&y.als).then(x.luma.cmp(&y.luma)));

        if self.stateful {
            self.data.save().expect("Unable to save data");
        }
    }

    async fn predict(&mut self, als: u64, luma: u8) {
        if let Some(prediction) = super::interpolate(&self.data.entries, self.scale, als, luma) {
            log::trace!(
                "[{}] Prediction: {prediction} (als: {als}, luma: {luma})",
                self.output_name
            );
            self.prediction_tx
                .send(prediction)
                .await
                .expect("Unable to send predicted brightness value, channel is dead");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Result;
    use macro_rules_attribute::apply;
    use smol::channel;
    use smol_macros::test;

    const ALS_DARK: u64 = 10;
    const ALS_DIM: u64 = 20;
    const ALS_BRIGHT: u64 = 30;

    async fn setup() -> Result<(Controller, Sender<u64>, Receiver<u64>)> {
        let (als_tx, als_rx) = channel::bounded(128);
        let (user_tx, user_rx) = channel::bounded(128);
        let (prediction_tx, prediction_rx) = channel::bounded(128);
        als_tx
            .send(Reading {
                value: ALS_BRIGHT,
                stable: true,
            })
            .await?;
        user_tx.send(0).await?;
        let controller = Controller::new(
            prediction_tx,
            user_rx,
            als_rx,
            false,
            "Dell 1",
            Scale::Linear,
            &HashMap::new(),
        );
        Ok((controller, user_tx, prediction_rx))
    }

    #[apply(test!)]
    async fn test_process_first_user_change() -> Result<()> {
        let (mut controller, user_tx, _) = setup().await?;

        // User changes brightness to value 33 for a given ALS and luma
        user_tx.send(33).await?;
        controller.process(ALS_DIM, 66).await;

        assert_eq!(Some(Entry::new(ALS_DIM, 66, 33)), controller.pending);
        assert!(controller.pending_cooldown.is_active());

        Ok(())
    }

    #[apply(test!)]
    async fn test_process_several_continuous_user_changes() -> Result<()> {
        let (mut controller, user_tx, _) = setup().await?;

        // User initiates brightness change for a given ALS and luma to value 33...
        user_tx.send(33).await?;
        controller.process(ALS_DIM, 66).await;
        // then quickly continues increasing it to 34 (while ALS and luma might already be different)...
        user_tx.send(34).await?;
        controller.process(ALS_BRIGHT, 36).await;
        // and even faster to 36 (which is the indended brightness value they wish to learn for the initial ALS and luma)
        user_tx.send(35).await?;
        user_tx.send(36).await?;
        controller.process(ALS_DARK, 16).await;

        assert_eq!(Some(Entry::new(ALS_DARK, 16, 36)), controller.pending);
        assert!(controller.pending_cooldown.is_active());

        Ok(())
    }

    #[apply(test!)]
    async fn test_process_learns_user_change_after_cooldown() -> Result<()> {
        let (mut controller, user_tx, _) = setup().await?;

        // User changes brightness to a desired value
        user_tx.send(33).await?;
        controller.process(ALS_DIM, 66).await;
        user_tx.send(33).await?;
        controller.process(ALS_BRIGHT, 64).await;
        user_tx.send(35).await?;
        controller.process(ALS_DARK, 62).await;

        // User doesn't change brightness anymore, so even if ALS or luma change, we are in cooldown period
        controller.process(ALS_BRIGHT, 60).await;
        assert!(controller.pending_cooldown.is_active());
        assert_eq!(Some(Entry::new(ALS_BRIGHT, 62, 35)), controller.pending);

        // One final process will trigger the learning
        controller.pending_cooldown.finish();
        controller.process(ALS_DARK, 61).await;

        assert_eq!(None, controller.pending);
        assert!(!controller.pending_cooldown.is_active());
        assert_eq!(vec![Entry::new(ALS_DARK, 62, 35)], controller.data.entries);

        Ok(())
    }

    #[apply(test!)]
    async fn unstable_als_prevents_prediction() -> Result<()> {
        let (mut controller, _, prediction_rx) = setup().await?;
        controller.data.entries = vec![Entry::new(ALS_DIM, 20, 30)];

        controller
            .process_reading(
                Reading {
                    value: ALS_DIM,
                    stable: false,
                },
                20,
            )
            .await;

        assert!(prediction_rx.is_empty());
        Ok(())
    }

    #[apply(test!)]
    async fn unstable_als_delays_learning_after_cooldown() -> Result<()> {
        let (mut controller, user_tx, _) = setup().await?;
        user_tx.send(33).await?;
        controller.process(ALS_DIM, 20).await;
        controller
            .process_reading(
                Reading {
                    value: ALS_DIM,
                    stable: false,
                },
                20,
            )
            .await;
        controller.pending_cooldown.finish();
        controller
            .process_reading(
                Reading {
                    value: ALS_DIM,
                    stable: false,
                },
                20,
            )
            .await;
        assert!(controller.pending.is_some());
        assert!(controller.data.entries.is_empty());

        controller
            .process_reading(
                Reading {
                    value: ALS_BRIGHT,
                    stable: true,
                },
                20,
            )
            .await;

        assert_eq!(None, controller.pending);
        assert_eq!(
            vec![Entry::new(ALS_BRIGHT, 20, 33)],
            controller.data.entries
        );
        Ok(())
    }

    // If user configured brightness value in certain conditions (amount of light around, screen contents),
    // how changes in environment or screen contents can affect the desired brightness level:
    //
    // |                 | darker env      | same env         | brighter env     |
    // | darker screen   | any             | same or brighter | same or brighter |
    // | same screen     | same or dimmer  | only same        | same or brighter |
    // | brighter screen | same or dimmer  | same or dimmer   | any              |

    #[apply(test!)]
    async fn test_learn_data_cleanup() -> Result<()> {
        let (mut controller, _, _) = setup().await?;
        controller.data.entries = vec![
            Entry::new(21, 20, 30),
            Entry::new(10, 30, 31),
            Entry::new(30, 10, 29),
            Entry::new(10, 30, 29),
            Entry::new(30, 10, 31),
            Entry::new(10, 10, 30),
            Entry::new(30, 30, 30),
        ];
        controller.pending = Some(Entry::new(ALS_DIM, 20, 30));

        controller.learn();

        assert_eq!(
            vec![
                Entry::new(10, 10, 30),
                Entry::new(10, 30, 29),
                Entry::new(20, 20, 30),
                Entry::new(30, 10, 31),
                Entry::new(30, 30, 30),
            ],
            controller.data.entries
        );
        Ok(())
    }

    #[apply(test!)]
    async fn test_learn_cleanup_when_brightness_decreases_as_als_increases() -> Result<()> {
        let (controller, _, _) = setup().await?;
        let mut controller = controller.with_als_direction(AlsDirection::Decreasing);
        controller.data.entries = vec![
            Entry::new(ALS_DARK, 20, 31),
            Entry::new(ALS_DARK, 20, 29),
            Entry::new(ALS_BRIGHT, 20, 29),
            Entry::new(ALS_BRIGHT, 20, 31),
        ];
        controller.pending = Some(Entry::new(ALS_DIM, 20, 30));

        controller.learn();

        assert_eq!(
            vec![
                Entry::new(ALS_DARK, 20, 31),
                Entry::new(ALS_DIM, 20, 30),
                Entry::new(ALS_BRIGHT, 20, 29),
            ],
            controller.data.entries
        );
        Ok(())
    }

    #[apply(test!)]
    async fn test_predict_no_data_points() -> Result<()> {
        let (mut controller, _, prediction_rx) = setup().await?;
        controller.data.entries = vec![];

        // predict() should not be called with no data, but just in case confirm we don't panic
        controller.predict(ALS_DIM, 20).await;

        assert!(prediction_rx.try_recv().is_err());

        Ok(())
    }

    #[apply(test!)]
    async fn test_predict_rejects_distant_data() -> Result<()> {
        let (mut controller, _, prediction_rx) = setup().await?;
        controller.data.entries = vec![Entry::new(100, 100, 100)];

        // predict() should not be called with no nearby data, but just in case confirm we don't panic
        controller.predict(ALS_DIM, 20).await;

        assert!(prediction_rx.try_recv().is_err());

        Ok(())
    }

    #[apply(test!)]
    async fn test_predict_one_data_point() -> Result<()> {
        let (mut controller, _, prediction_rx) = setup().await?;
        controller.data.entries = vec![Entry::new(ALS_DIM, 10, 15)];

        controller.predict(ALS_DIM, 20).await;

        assert_eq!(15, prediction_rx.try_recv()?);
        Ok(())
    }

    #[apply(test!)]
    async fn test_predict_known_conditions() -> Result<()> {
        let (mut controller, _, prediction_rx) = setup().await?;
        controller.data.entries = vec![Entry::new(ALS_DIM, 10, 15), Entry::new(ALS_DIM, 20, 30)];

        controller.predict(ALS_DIM, 20).await;

        assert_eq!(30, prediction_rx.try_recv()?);
        Ok(())
    }

    #[apply(test!)]
    async fn test_predict_approximate() -> Result<()> {
        let (mut controller, _, prediction_rx) = setup().await?;
        controller.data.entries = vec![
            Entry::new(ALS_DIM, 10, 15),
            Entry::new(ALS_DIM, 20, 30),
            Entry::new(ALS_DIM, 100, 100),
        ];

        // Approximated using weighted distance to all known points:
        // dist1 = sqrt((x1 - x2)^2 + (y1 - y2)^2)
        // weight1 = (1/dist1^2) / (1/dist1^2 + 1/dist2^2 + 1/dist3^2)
        // prediction = weight1*brightness1 + weight2*brightness2 + weight3*brightness
        controller.predict(ALS_DIM, 50).await;

        assert_eq!(38, prediction_rx.try_recv()?);
        Ok(())
    }

    #[apply(test!)]
    async fn test_predict_uses_local_continuous_data() -> Result<()> {
        let (mut controller, _, prediction_rx) = setup().await?;
        controller.data.entries = vec![
            Entry::new(ALS_DIM, 10, 15),
            Entry::new(ALS_DIM, 20, 30),
            Entry::new(ALS_DIM, 100, 100),
            Entry::new(ALS_DARK, 50, 100),
            Entry::new(ALS_BRIGHT, 51, 100),
        ];

        controller.predict(ALS_DIM, 50).await;

        assert_eq!(94, prediction_rx.try_recv()?);
        Ok(())
    }
}
