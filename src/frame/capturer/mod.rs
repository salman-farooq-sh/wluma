pub mod none;
pub mod pipewire;
pub mod wayland;

#[allow(clippy::large_enum_variant)]
pub enum Capturer {
    Auto,
    None(none::Capturer),
    Pipewire(crate::config::PipewireProtocol),
    Wayland(wayland::Capturer),
}

impl Capturer {
    pub async fn run(self, output_name: &str, controller: crate::predictor::Controller) {
        match self {
            Capturer::Auto => {
                let output = output_name.to_string();
                smol::unblock(move || {
                    match wayland::Capturer::is_supported() {
                        Ok(true) => {
                            log::debug!("Auto capturer selected Wayland for '{output}'");
                            wayland::Capturer::new(crate::config::WaylandProtocol::Any)
                                .run(&output, controller);
                            return;
                        }
                        Ok(false) => {
                            log::debug!("Auto capturer found no supported Wayland capture protocol");
                        }
                        Err(error) => {
                            log::debug!("Auto capturer could not inspect Wayland protocols: {error:#}");
                        }
                    }

                    match pipewire::prepare(&output, crate::config::PipewireProtocol::Any) {
                        Ok(source) => {
                            log::debug!("Auto capturer selected PipeWire for '{output}'");
                            pipewire::run_prepared(source, controller);
                        }
                        Err(error) => {
                            log::warn!(
                                "No supported screen capture protocol found for '{output}', using ALS only: {error:#}"
                            );
                            smol::block_on(none::Capturer::default().run(&output, controller));
                        }
                    }
                })
                .await;
            }
            Capturer::None(mut c) => c.run(output_name, controller).await,
            Capturer::Pipewire(protocol) => {
                let output = output_name.to_string();
                smol::unblock(move || pipewire::run(&output, protocol, controller)).await;
            }
            Capturer::Wayland(mut c) => {
                let output = output_name.to_string();
                smol::unblock(move || c.run(&output, controller)).await;
            }
        }
    }
}
