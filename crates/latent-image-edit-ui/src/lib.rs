//! egui front end of [`latent_image_edit`]: the image editor window ([`Editor`]), the slider
//! groups and presets, and the widgets they are made of (form rows, zoomable canvas, crop
//! overlay, histogram). Knows nothing about scanners, so any host can embed it.

pub mod canvas;
pub mod crop;
pub mod dust;
pub mod edit;
pub mod editor;
pub mod form;
pub mod histogram;
pub mod presets;
pub mod preview;
pub mod sliders;
pub mod trace;
pub mod view;

pub use editor::{Editor, EditorEvent};

use egui::Color32;

pub const ACCENT: Color32 = Color32::from_rgb(70, 130, 200);
