use futures_util::stream::{self, StreamExt};
use macro_rules_attribute::apply;
use smol::channel;

mod als;
mod brightness;
mod channel_ext;
mod config;
mod device_file;
mod frame;
mod predictor;
mod state;

/// Current app version (determined at compile-time).
pub const VERSION: &str = env!("WLUMA_VERSION");

#[apply(smol_macros::main!)]
async fn main() {
    let panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        panic_hook(panic_info);
        std::process::exit(1);
    }));

    env_logger::builder()
        .filter_level(log::LevelFilter::Info)
        .parse_default_env()
        .init();

    log::debug!("== wluma v{} ==", VERSION);

    match state::migrate() {
        Ok(true) => log::info!(
            "Learned data has been migrated from $XDG_DATA_HOME/wluma to $XDG_STATE_HOME/wluma."
        ),
        Ok(false) => {}
        Err(error) => panic!("Unable to migrate data files to the XDG state directory: {error:#}"),
    }

    let config = match config::load() {
        Ok(config) => config,
        Err(err) => panic!("Unable to load config: {}", err),
    };

    log::debug!("Using {:#?}", config);

    let (als_scale, legacy_thresholds) = match &config.als {
        config::Als::Auto { thresholds } | config::Als::Iio { thresholds, .. } => {
            (als::Scale::Lux, thresholds.clone())
        }
        config::Als::External {
            scale, thresholds, ..
        } => (*scale, thresholds.clone()),
        config::Als::Webcam { thresholds, .. } | config::Als::Time { thresholds, .. } => {
            (als::Scale::Linear, thresholds.clone())
        }
        config::Als::None => (als::Scale::Linear, Default::default()),
    };

    let (mut tasks, als_txs) = stream::iter(config.output.clone())
        .fold(
            (Vec::new(), Vec::new()),
            |(mut tasks, mut als_txs), output| {
                let legacy_thresholds = legacy_thresholds.clone();
                async move {
                    let (als_tx, als_rx) = channel::bounded(128);
                    let (user_tx, user_rx) = channel::bounded(128);
                    let (prediction_tx, prediction_rx) = channel::bounded(128);

                    let (
                        output_name,
                        output_capturer,
                        vulkan_device,
                        output_predictor,
                        als_direction,
                    ) = match &output {
                        config::Output::Backlight(cfg) => (
                            cfg.name.clone(),
                            cfg.capturer.clone(),
                            cfg.vulkan_device.clone(),
                            cfg.predictor.clone(),
                            cfg.als_direction,
                        ),
                        config::Output::DdcUtil(cfg) => (
                            cfg.name.clone(),
                            cfg.capturer.clone(),
                            cfg.vulkan_device.clone(),
                            cfg.predictor.clone(),
                            predictor::AlsDirection::Increasing,
                        ),
                    };

                    let brightness = match output {
                        config::Output::Backlight(cfg) => {
                            brightness::Backlight::new(&cfg.path, cfg.min_brightness)
                                .await
                                .map(brightness::Brightness::Backlight)
                        }
                        config::Output::DdcUtil(cfg) => {
                            brightness::DdcUtil::new(&cfg.identifier, cfg.min_brightness)
                                .map(brightness::Brightness::DdcUtil)
                        }
                    };

                    match brightness {
                        Ok(b) => {
                            tasks.push(smol::spawn(async {
                                brightness::Controller::new(b, user_tx, prediction_rx)
                                    .run()
                                    .await;
                            }));

                            tasks.push(smol::spawn(async move {
                                let frame_capturer: frame::capturer::Capturer =
                                    match output_capturer {
                                        config::Capturer::Auto => frame::capturer::Capturer::Auto,
                                        config::Capturer::Wayland(protocol) => {
                                            frame::capturer::Capturer::Wayland(
                                                frame::capturer::wayland::Capturer::new(protocol),
                                            )
                                        }
                                        config::Capturer::Pipewire(protocol) => {
                                            frame::capturer::Capturer::Pipewire(protocol)
                                        }
                                        config::Capturer::None => {
                                            frame::capturer::Capturer::None(Default::default())
                                        }
                                    };

                                let controller = match output_predictor {
                                    config::Predictor::Manual { points } => {
                                        predictor::Controller::Manual(
                                            predictor::controller::manual::Controller::new(
                                                prediction_tx,
                                                user_rx,
                                                als_rx,
                                                points
                                                    .into_iter()
                                                    .map(|point| {
                                                        predictor::Entry::new(
                                                            point.als,
                                                            point.luma,
                                                            point.reduction,
                                                        )
                                                    })
                                                    .collect(),
                                                &output_name,
                                                als_scale,
                                            ),
                                        )
                                    }
                                    config::Predictor::Adaptive => predictor::Controller::Adaptive(
                                        predictor::controller::adaptive::Controller::new(
                                            prediction_tx,
                                            user_rx,
                                            als_rx,
                                            true,
                                            &output_name,
                                            als_scale,
                                            &legacy_thresholds,
                                        )
                                        .with_als_direction(als_direction),
                                    ),
                                };

                                frame_capturer
                                    .run(&output_name, controller, vulkan_device.as_deref())
                                    .await;
                            }));

                            als_txs.push(als_tx);
                        }
                        Err(err) => {
                            log::warn!(
                                "Skipping '{}' as it might be disconnected: {}",
                                output_name,
                                err
                            );
                        }
                    }

                    (tasks, als_txs)
                }
            },
        )
        .await;

    let als: als::Als = match config.als {
        config::Als::Auto { .. } => match als::iio::Als::new(Some("/sys/bus/iio/devices")).await {
            Ok(sensor) => als::Als::Iio(sensor),
            Err(error) => {
                log::info!("No ambient light sensor detected, continuing without one: {error}");
                als::Als::None(Default::default())
            }
        },
        config::Als::External { path, scale, .. } => {
            als::Als::External(als::external::Als::new(path, scale))
        }
        config::Als::Iio { path, .. } => als::Als::Iio(
            als::iio::Als::new(path.as_deref())
                .await
                .expect("Unable to initialize ambient light sensor"),
        ),
        config::Als::Time { levels, .. } => als::Als::Time(
            als::time::Als::new(levels).expect("Unable to initialize time-based ambient light"),
        ),
        config::Als::Webcam { video, .. } => als::Als::Webcam({
            let (webcam_tx, webcam_rx) = channel::bounded(128);
            // TODO: make async
            tasks.push(smol::unblock(move || {
                als::webcam::Webcam::new(webcam_tx, video).run();
            }));
            als::webcam::Als::new(webcam_rx)
        }),
        config::Als::None => als::Als::None(Default::default()),
    };

    tasks.push(smol::spawn(async {
        als::controller::Controller::new(als, als_txs).run().await;
    }));

    log::info!("Continue adjusting brightness and wluma will learn your preference over time.");

    futures_util::future::join_all(tasks).await;
}
