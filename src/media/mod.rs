//! Images: validation and metadata stripping ([`image`]) and the rules for
//! sending them to a model ([`vision`]).

pub mod image;
pub mod vision;

pub use image::{Format, LoadedImage};
pub use vision::{Route, VisionSettings};
