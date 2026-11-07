use std::path::PathBuf;

struct Gui {
    scan: latent_scan_ui::ScanUi,
    styled: bool,
}

impl eframe::App for Gui {
    fn update(&mut self, ctx: &eframe::egui::Context, _: &mut eframe::Frame) {
        if !self.styled {
            latent_scan_ui::apply_theme(ctx);
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
            // With vsync the EGL swap waits for the compositor's frame callback, which a window on
            // another workspace never gets: the event loop then stalls inside the swap, misses the
            // compositor's ping and the desktop says "not responding" (seen on Hyprland during a
            // scan, when progress keeps asking for redraws). egui draws on demand, so nothing busy-loops.
            // `LATENT_SCAN_VSYNC=1` turns it back on (tearing on X11).
            vsync: std::env::var_os("LATENT_SCAN_VSYNC").is_some_and(|v| !v.is_empty()),
            ..Default::default()
        },
        Box::new(move |cc| {
            let mut scan = latent_scan_ui::ScanUi::standalone(archive);
            scan.open(&cc.egui_ctx, None);
            Ok(Box::new(Gui {
                scan,
                styled: false,
            }))
        }),
    )
}
