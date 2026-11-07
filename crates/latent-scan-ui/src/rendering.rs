//! Renders the scan overview on its own thread so a slider drag never blocks the window. Only
//! the newest request is rendered; older ones that are still waiting are dropped.

use crate::preview::{self, Look};
use egui::{ColorImage, Context};
use latent_scan_core::RawImage;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

/// What the preview should show: the look and the frame the tone is estimated from.
pub type Key = (Look, usize, [f32; 4]);

pub struct Job {
    /// Which overview the job belongs to (a new preview scan starts a new one).
    pub serial: u64,
    pub key: Key,
    pub raw: Arc<RawImage>,
}

pub struct Done {
    pub serial: u64,
    pub key: Key,
    pub image: ColorImage,
}

pub struct PreviewRenderer {
    tx: Sender<Job>,
    rx: Receiver<Done>,
}

impl PreviewRenderer {
    pub fn new(ctx: &Context) -> Self {
        let (tx, jobs) = channel::<Job>();
        let (done, rx) = channel();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            while let Ok(mut job) = jobs.recv() {
                while let Ok(newer) = jobs.try_recv() {
                    job = newer;
                }
                let (look, _, frame) = job.key;
                let image = latent_image_edit_ui::trace::timed("scan preview render", || {
                    preview::render(&job.raw, look, frame)
                });
                let out = Done {
                    serial: job.serial,
                    key: job.key,
                    image,
                };
                if done.send(out).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        Self { tx, rx }
    }

    /// Returns false when the thread is gone.
    pub fn request(&self, job: Job) -> bool {
        self.tx.send(job).is_ok()
    }

    /// The finished previews, oldest first.
    pub fn finished(&self) -> impl Iterator<Item = Done> + '_ {
        self.rx.try_iter()
    }
}
