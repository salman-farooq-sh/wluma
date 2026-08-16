pub mod none;
pub mod pipewire;
pub mod wayland;

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

#[allow(clippy::large_enum_variant)]
pub enum Capturer {
    Auto,
    None(none::Capturer),
    Pipewire(crate::config::PipewireProtocol),
    Wayland(wayland::Capturer),
}

impl Capturer {
    pub async fn run(
        self,
        output_name: &str,
        controller: crate::predictor::Controller,
        vulkan_device: Option<&str>,
        active: Arc<AtomicBool>,
        status: crate::control::Hub,
    ) {
        match self {
            Capturer::Auto => {
                let output = output_name.to_string();
                let vulkan_device = vulkan_device.map(str::to_string);
                smol::unblock(move || {
                    match wayland::Capturer::is_supported() {
                        Ok(true) => {
                            status.set_capturer(&output, "wayland");
                            log::debug!("Auto capturer selected Wayland for '{output}'");
                            wayland::Capturer::new(crate::config::WaylandProtocol::Any).run(
                                &output,
                                controller,
                                vulkan_device.as_deref(),
                                active,
                            );
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
                            status.set_capturer(&output, "pipewire");
                            log::debug!("Auto capturer selected PipeWire for '{output}'");
                            pipewire::run_prepared(
                                source,
                                controller,
                                vulkan_device.as_deref(),
                                active,
                            );
                        }
                        Err(error) => {
                            status.set_capturer(&output, "none");
                            log::warn!(
                                "No supported screen capture protocol found for '{output}', using ALS only: {error:#}"
                            );
                            smol::block_on(none::Capturer::default().run(&output, controller, active));
                        }
                    }
                })
                .await;
            }
            Capturer::None(mut c) => {
                status.set_capturer(output_name, "none");
                c.run(output_name, controller, active).await
            }
            Capturer::Pipewire(protocol) => {
                status.set_capturer(output_name, "pipewire");
                let output = output_name.to_string();
                let vulkan_device = vulkan_device.map(str::to_string);
                smol::unblock(move || {
                    pipewire::run(
                        &output,
                        protocol,
                        controller,
                        vulkan_device.as_deref(),
                        active,
                    )
                })
                .await;
            }
            Capturer::Wayland(mut c) => {
                status.set_capturer(output_name, "wayland");
                let output = output_name.to_string();
                let vulkan_device = vulkan_device.map(str::to_string);
                smol::unblock(move || c.run(&output, controller, vulkan_device.as_deref(), active))
                    .await;
            }
        }
    }
}
