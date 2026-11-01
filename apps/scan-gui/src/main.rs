use std::path::PathBuf;

struct Gui {
    scan: scan_ui::ScanUi,
    styled: bool,
}

impl eframe::App for Gui {
    fn update(&mut self, ctx: &eframe::egui::Context, _: &mut eframe::Frame) {
        if !self.styled {
            scan_ui::apply_theme(ctx);
            self.styled = true;
        }
        // Saved scans are already on disk; this app has nothing else to update.
        let _ = self.scan.show_main(ctx);
    }
}

fn main() -> eframe::Result {
    // Without an argument the app saves where it saved last time (a plain folder at first); an
    // argument names a latent archive to scan into.
    let archive: Option<PathBuf> = std::env::args_os().nth(1).map(PathBuf::from);
    eframe::run_native(
        "Latent Scan",
        eframe::NativeOptions {
            viewport: eframe::egui::ViewportBuilder::default().with_inner_size([1280.0, 800.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            let mut scan = scan_ui::ScanUi::standalone(archive);
            scan.open(&cc.egui_ctx, None);
            Ok(Box::new(Gui {
                scan,
                styled: false,
            }))
        }),
    )
}
