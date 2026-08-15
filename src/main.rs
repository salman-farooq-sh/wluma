use macro_rules_attribute::apply;
use smol::channel;

mod als;
mod brightness;
mod channel_ext;
mod cli;
mod config;
mod control;
mod device_file;
mod frame;
mod predictor;
mod runtime;
mod state;

/// Current app version (determined at compile-time).
pub const VERSION: &str = env!("WLUMA_VERSION");

#[apply(smol_macros::main!)]
async fn main() {
    match cli::parse() {
        Ok(cli::Mode::Daemon) => {}
        Ok(cli::Mode::Command { request, stream }) => {
            if let Err(error) = control::send(&request, stream).await {
                eprintln!("wluma: {error:#}");
                std::process::exit(1);
            }
            return;
        }
        Ok(cli::Mode::Print(value)) => {
            println!("{value}");
            return;
        }
        Err(error) => {
            eprintln!("wluma: {error}");
            std::process::exit(2);
        }
    }

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

    let _instance_lock = control::InstanceLock::acquire()
        .unwrap_or_else(|error| panic!("Unable to acquire daemon lock: {error:#}"));

    match state::migrate() {
        Ok(true) => log::info!(
            "Learned data has been migrated from $XDG_DATA_HOME/wluma to $XDG_STATE_HOME/wluma."
        ),
        Ok(false) => {}
        Err(error) => panic!("Unable to migrate data files to the XDG state directory: {error:#}"),
    }

    let config = match config::load() {
        Ok(config) => config,
        Err(error) => panic!("Unable to load config: {error}"),
    };

    log::debug!("Using {:#?}", config);

    let als_kind = match &config.als {
        config::Als::Auto { .. } => "auto",
        config::Als::External { .. } => "external",
        config::Als::Iio { .. } => "iio",
        config::Als::Time { .. } => "time",
        config::Als::Webcam { .. } => "webcam",
        config::Als::None => "none",
    };
    let status = control::Hub::new(als_kind);

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

    let mut webcam_task = None;
    let source = match config.als {
        config::Als::Auto { .. } => als::Als::Auto(Default::default()),
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
            webcam_task = Some(smol::unblock(move || {
                als::webcam::Webcam::new(webcam_tx, video).run();
            }));
            als::webcam::Als::new(webcam_rx)
        }),
        config::Als::None => als::Als::None(Default::default()),
    };

    let (registration_tx, registration_rx) = channel::unbounded();
    let als_status = status.clone();
    let als_task = smol::spawn(async move {
        als::controller::Controller::new(source, registration_rx)
            .with_status(als_status)
            .run()
            .await;
    });

    let (control_tx, control_rx) = channel::unbounded();
    let control_status = status.clone();
    let control_task = smol::spawn(async move {
        if let Err(error) = control::serve(control_status, control_tx).await {
            panic!("Unable to start control socket: {error:#}");
        }
    });

    log::info!("Continue adjusting brightness and wluma will learn your preference over time.");

    let mut runtime = runtime::Runtime::new(
        config.output,
        als_scale,
        legacy_thresholds,
        registration_tx,
        control_rx,
        status,
    );
    runtime.run().await;
    drop(control_task);
    drop(als_task);
    drop(webcam_task);
}
