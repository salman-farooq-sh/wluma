pub mod controller;
mod data;
pub use controller::Controller;
pub use data::Entry;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlsDirection {
    Increasing,
    Decreasing,
}
