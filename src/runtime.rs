use crate::{als, brightness, config, frame, predictor};
use anyhow::Result;
use smol::channel::{self, Sender};
use smol::Task;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TOPOLOGY_POLL_INTERVAL: Duration = Duration::from_secs(1);
const TOPOLOGY_SETTLE_INTERVAL: Duration = Duration::from_secs(2);
const DISCOVERY_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const START_RETRY_INTERVAL: Duration = Duration::from_secs(5);

pub struct Runtime {
    configured: Vec<config::Output>,
    als_scale: als::Scale,
    legacy_thresholds: HashMap<u64, String>,
    registrations: Sender<Sender<als::Reading>>,
    topology: Vec<String>,
    desired: HashMap<String, config::Output>,
    sessions: HashMap<String, Session>,
    failures: HashMap<String, Instant>,
    last_discovery: Option<Instant>,
    settling_until: Option<Instant>,
}

struct Session {
    output: config::Output,
    active: Arc<AtomicBool>,
    brightness: Task<()>,
    capturer: Task<()>,
}

impl Runtime {
    pub fn new(
        configured: Vec<config::Output>,
        als_scale: als::Scale,
        legacy_thresholds: HashMap<u64, String>,
        registrations: Sender<Sender<als::Reading>>,
    ) -> Self {
        Self {
            configured,
            als_scale,
            legacy_thresholds,
            registrations,
            topology: Vec::new(),
            desired: HashMap::new(),
            sessions: HashMap::new(),
            failures: HashMap::new(),
            last_discovery: None,
            settling_until: None,
        }
    }

    pub async fn run(&mut self) {
        loop {
            self.step().await;
            smol::Timer::after(TOPOLOGY_POLL_INTERVAL).await;
        }
    }

    async fn step(&mut self) {
        let topology = config::topology();
        let topology_changed = topology != self.topology;
        let initial = self.last_discovery.is_none();
        let settled = self
            .settling_until
            .is_some_and(|deadline| Instant::now() >= deadline);
        let refresh = self
            .last_discovery
            .is_none_or(|last| last.elapsed() >= DISCOVERY_REFRESH_INTERVAL);

        if topology_changed {
            self.topology = topology;
            if !initial {
                self.settling_until = Some(Instant::now() + TOPOLOGY_SETTLE_INTERVAL);
            }
            self.discover().await;
        } else if settled {
            self.settling_until = None;
            self.discover().await;
        } else if self.settling_until.is_none() && refresh {
            self.discover().await;
        }
        self.reconcile().await;
    }

    async fn discover(&mut self) {
        let configured = self.configured.clone();
        let outputs = smol::unblock(move || config::detected_outputs(configured)).await;
        self.last_discovery = Some(Instant::now());
        let mut desired = HashMap::new();
        for output in outputs {
            let name = output_name(&output).to_string();
            if desired.contains_key(&name) {
                log::warn!("Skipping duplicate discovered output '{name}'");
                continue;
            }
            desired.insert(name, output);
        }
        for (name, output) in &desired {
            if self.desired.get(name) != Some(output) {
                log_discovered(output);
            }
        }
        self.desired = desired;
    }

    async fn reconcile(&mut self) {
        let stopped = self
            .sessions
            .iter()
            .filter(|(_, session)| {
                session.brightness.is_finished() || session.capturer.is_finished()
            })
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in stopped {
            if let Some(session) = self.sessions.remove(&name) {
                session.stop().await;
                log::warn!("Output '{name}' stopped unexpectedly; it will be retried");
                self.failures.insert(name, Instant::now());
            }
        }

        let removed = self
            .sessions
            .iter()
            .filter(|(name, session)| self.desired.get(*name) != Some(&session.output))
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>();
        for name in removed {
            if let Some(session) = self.sessions.remove(&name) {
                session.stop().await;
                log::info!("Stopped using output '{name}'");
            }
            self.failures.remove(&name);
        }

        let pending = self
            .desired
            .iter()
            .filter(|(name, _)| !self.sessions.contains_key(*name))
            .filter(|_| self.settling_until.is_none())
            .filter(|(name, _)| {
                self.failures
                    .get(*name)
                    .is_none_or(|failed| failed.elapsed() >= START_RETRY_INTERVAL)
            })
            .map(|(name, output)| (name.clone(), output.clone()))
            .collect::<Vec<_>>();
        for (name, output) in pending {
            match Session::start(
                output,
                self.als_scale,
                &self.legacy_thresholds,
                &self.registrations,
            )
            .await
            {
                Ok(session) => {
                    log::info!("Started using output '{name}'");
                    self.sessions.insert(name.clone(), session);
                    self.failures.remove(&name);
                }
                Err(error) => {
                    log::warn!("Unable to initialize output '{name}': {error:#}");
                    self.failures.insert(name, Instant::now());
                }
            }
        }
    }
}

impl Session {
    async fn start(
        output: config::Output,
        als_scale: als::Scale,
        legacy_thresholds: &HashMap<u64, String>,
        registrations: &Sender<Sender<als::Reading>>,
    ) -> Result<Self> {
        let (als_tx, als_rx) = channel::bounded(1);
        let (user_tx, user_rx) = channel::bounded(128);
        let (prediction_tx, prediction_rx) = channel::bounded(128);

        let (name, capturer, vulkan_device, output_predictor, als_direction) = match &output {
            config::Output::Backlight(output) => (
                output.name.clone(),
                output.capturer.clone(),
                output.vulkan_device.clone(),
                output.predictor.clone(),
                output.als_direction,
            ),
            config::Output::DdcUtil(output) => (
                output.name.clone(),
                output.capturer.clone(),
                output.vulkan_device.clone(),
                output.predictor.clone(),
                predictor::AlsDirection::Increasing,
            ),
        };

        let backend = match &output {
            config::Output::Backlight(output) => {
                brightness::Backlight::new(&output.path, output.min_brightness)
                    .await
                    .map(brightness::Brightness::Backlight)?
            }
            config::Output::DdcUtil(output) => {
                let identifier = output.identifier.clone();
                let min_brightness = output.min_brightness;
                brightness::Brightness::DdcUtil(
                    smol::unblock(move || brightness::DdcUtil::new(&identifier, min_brightness))
                        .await?,
                )
            }
        };

        registrations.send(als_tx).await?;
        let brightness = smol::spawn(async move {
            brightness::Controller::new(backend, user_tx, prediction_rx)
                .run()
                .await;
        });

        let controller = match output_predictor {
            config::Predictor::Manual { points } => {
                predictor::Controller::Manual(predictor::controller::manual::Controller::new(
                    prediction_tx,
                    user_rx,
                    als_rx,
                    points
                        .into_iter()
                        .map(|point| predictor::Entry::new(point.als, point.luma, point.reduction))
                        .collect(),
                    &name,
                    als_scale,
                ))
            }
            config::Predictor::Adaptive => predictor::Controller::Adaptive(
                predictor::controller::adaptive::Controller::new(
                    prediction_tx,
                    user_rx,
                    als_rx,
                    true,
                    &name,
                    als_scale,
                    legacy_thresholds,
                )
                .with_als_direction(als_direction),
            ),
        };
        let frame_capturer = match capturer {
            config::Capturer::Auto => frame::capturer::Capturer::Auto,
            config::Capturer::Wayland(protocol) => frame::capturer::Capturer::Wayland(
                frame::capturer::wayland::Capturer::new(protocol),
            ),
            config::Capturer::Pipewire(protocol) => frame::capturer::Capturer::Pipewire(protocol),
            config::Capturer::None => frame::capturer::Capturer::None(Default::default()),
        };
        let active = Arc::new(AtomicBool::new(true));
        let capture_active = active.clone();
        let capturer = smol::spawn(async move {
            frame_capturer
                .run(&name, controller, vulkan_device.as_deref(), capture_active)
                .await;
        });

        Ok(Self {
            output,
            active,
            brightness,
            capturer,
        })
    }

    async fn stop(self) {
        self.active.store(false, Ordering::Relaxed);
        self.brightness.cancel().await;
        self.capturer.await;
    }
}

fn output_name(output: &config::Output) -> &str {
    match output {
        config::Output::Backlight(output) => &output.name,
        config::Output::DdcUtil(output) => &output.name,
    }
}

fn log_discovered(output: &config::Output) {
    match output {
        config::Output::Backlight(output) if output.path.contains("/class/leds/") => log::debug!(
            "Discovered keyboard '{}' using backlight {}",
            output.name,
            output.path
        ),
        config::Output::Backlight(output) => log::debug!(
            "Discovered output '{}' using backlight {}",
            output.name,
            output.path
        ),
        config::Output::DdcUtil(output) => log::debug!(
            "Discovered output '{}' using DDC identifier '{}'",
            output.name,
            output.identifier
        ),
    }
}
