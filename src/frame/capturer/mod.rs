pub mod none;
pub mod pipewire;
pub mod wayland;

#[allow(clippy::large_enum_variant)]
pub enum Capturer {
    None(none::Capturer),
    Pipewire,
    Wayland(wayland::Capturer),
}

impl Capturer {
    pub async fn run(self, output_name: &str, controller: crate::predictor::Controller) {
        match self {
            Capturer::None(mut c) => c.run(output_name, controller).await,
            Capturer::Pipewire => {
                let output = output_name.to_string();
                smol::unblock(move || pipewire::run(&output, controller)).await;
            }
            Capturer::Wayland(mut c) => {
                let output = output_name.to_string();
                smol::unblock(move || c.run(&output, controller)).await;
            }
        }
    }
}
