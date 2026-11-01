# latent-scan

Standalone film-scanning module for [latent](../latent) (Rust, egui archive of films and
enlargements). Built to be merged into latent later: crates here become workspace members there.

## Commands
- `nix develop` for the toolchain and `scanimage` (sane-backends).
- `cargo test --workspace`, `cargo clippy --workspace --all-targets`, `cargo fmt`.
- `cargo run -p scan-cli -- list | caps | profiles | scan -p <profile> [--archive DIR --table T --id N | -o FILE]`
- `cargo run -p scan-gui` for the GUI (saves to the folder used last, `~/Pictures/latent-scan` at first); `cargo run -p scan-gui -- <archive>` starts in that latent archive.

## Layout
- `crates/scan-core`: types and the `Scanner` / `ScannerBackend` traits. No I/O.
- `crates/scan-sane`: SANE backend driving `scanimage` (Canon `pixma`, Epson `epson2`/`epsonds`). Parsers are tested against `tests/fixtures/`.
- `crates/scan-profiles`: named presets from `profiles.toml` (source, colour, depth, dpi, area in mm).
- `crates/scan-pipeline`: `Stage` trait (crop, rotate, invert, tone) on 16-bit `RawImage`; `write_image` (TIFF/PNG).
- `crates/scan-archive`: latent folder layout `images/<table>/<id:04>/<file>` and appending to a row's `images` list in `latent.data.json`. On disk `images` is a JSON array of paths (latent only joins it with newlines in memory; a plain string fails to load with "bad json value"); `add_image` also converts such a string into a list. Mirrors `latent_data::fs::import_image`; keep in sync.
- `crates/scan-detect`: fits preset frames to the film (see "Auto-adjust"); pure, depends on `scan-core` only.
- `crates/scan-ui`: egui UI (`ScanUi`). `show_main(ctx)` fills the standalone app's window (resizable controls left, preview right, image strip bottom); `open`/`show(ctx)` is the floating window for embedding. Files: `lib.rs` (state and logic), `panels.rs` (controls, sliders, preview pane), `strip.rs` (image strip, large view), `thumbs.rs` (thumbnail cache), `settings.rs` (remembered settings), `crop.rs` (frame editing, `crop_to_mm`), `preview.rs` (overview rendering), `targets.rs` (rows and a row's images from `latent.data.json`), `worker.rs` (jobs on threads over an mpsc channel). Depends on `egui`; `scan-detect` stays free of it.
- `apps/scan-cli`: the `latent-scan` binary.
- `apps/scan-gui`: the `latent-scan-gui [archive]` eframe app hosting `ScanUi`.

## Conventions
- Backends only talk through `scan-core` traits; nothing outside `scan-sane` knows about SANE.
- Library errors are `thiserror` enums; the binary prints `error: ...` and exits 1.
- With pixma, set `--source` before mode and resolution (it resets them).
- Film-holder areas in `profiles.toml` are approximate and need calibrating. Epson V850 is untested (no hardware).
- 16-bit samples throughout; never downconvert before writing (JPEG, 8 bit by nature, is the one file format that cannot hold them).

## Embedding into latent
- Add the `scan-*` crates as native-only workspace members (`cfg(not(wasm32))`, like `vision`); add a File > "Scan..." item that calls `ScanUi::open(ctx, Some(Target { table, row_id }))` and `ScanUi::show(ctx)` each frame.
- latent keeps the store in memory, so construct with `.write_data(false)` and call `add_images` from `ScanEvent::Saved`; otherwise reload after the event.
- `crop.rs` mirrors `crop_overlay` in latent's `lib.rs`; extract one shared helper when merging.
- UI strings are English constants (`text` module in `scan-ui/src/lib.rs`); latent plans German later.
- Cancel: `CancelToken` is passed to `Scanner::scan`; the SANE backend sends SIGTERM to `scanimage` (SIGKILL after 10 s).

## Scanner behaviour (Canon 9000F, measured)
- Options differ per source: `Capabilities` holds one `SourceCaps` per source. Flatbed offers 75–4800 dpi and `48 bits color` / `16 bits gray`; the transparency unit offers 300–9600 dpi and only `Color` / `Gray` / `Infrared`, which deliver 16-bit samples. `pick_mode` falls back to the plain mode.
- The transparency window is about 216 x 250.6 mm although `-y` reports 297 mm; the scanner clips. The GUI therefore derives the preview's size from the pixels delivered (`delivered_area` in `scan-ui/src/worker.rs`).
- Flatbed 48-bit scans at 75 dpi come back at 150 dpi (driver minimum).
- 600 dpi A4 16-bit is ~215 MB; `read_tiff` decodes without the image crate's allocation limit.

## Calibrating film-holder areas
1. `cargo run -p scan-gui -- <archive>`, select the profile (e.g. `35mm-strip`), put the holder in, press Preview (whole transparency window).
2. Drag the frame onto the frame area, type a name under "Save profile" and press its button.
3. Stored per user in `~/.config/latent-scan/profiles.toml` (`$XDG_CONFIG_HOME` honoured); same-named profiles override the built-in ones from `crates/scan-profiles/profiles.toml`.

## Frames and batch scans
- A profile can hold several frames (`Profile::frames`, mm on the bed, scan order). `area` stays as the first frame for old files; `frame_areas()` / `jobs()` give one entry per frame.
- GUI: on the overview preview, "Add frame" puts a new frame next to the active one (same size, along the longer side); drag a frame to select and move it, drag its edges to resize, "Delete frame" removes it. "Save profile" stores all frames. Dimming is only drawn for a single frame.
- Scan runs one scan per frame at full dpi (`worker::run_batch`, tested with a fake `Scanner`). Files are `<prefix>-<NN>` from `scan_archive::naming::frame_stem`, numbered from "Start no."; the start advances by one per saved frame. A failed or cancelled batch keeps the frames already saved.
- `ScanUi::show` returns `Vec<ScanEvent>` (one `Saved` per frame).
- CLI: `scan -p <profile> --name <prefix> --start N --archive DIR --table T --id N` scans all frames of the profile; `-o` is only for single-frame profiles. Without `--start` a single frame is stored as `<name>.tiff`.

## Tone, colour mode and formats
- `scan_pipeline::tone`: `Tone::estimate(reference, ToneOptions)` + `Tone::apply`, and `ToneStage` (estimate from the image itself) for the scan pipeline (`Rotate -> ToneStage`).
  - **Negative**: per channel `1 - v/base`, base = 99.5th percentile of the reference (the thinnest density). Equalises the channels' bases, which removes the orange mask; plain invert for gray. Heuristic: a frame without any thin-density area is balanced slightly off.
  - **Auto levels**: per channel stretch of the 0.1th–99.8th percentile to the full range, after the negative step. Off by default so archive scans stay untouched.
- The GUI keeps the overview preview as a display-sized 16-bit `RawImage` and re-renders its texture (`preview::render`) when negative / auto exposure / colour mode change, the active frame changes or a drag ends. The tone is estimated from the active frame only and applied to the whole overview, so the preview shows what that frame's scan will look like. Preview scans are always colour; Gray is applied as luma in the display step.
- Colour mode (Colour / Gray) and the negative flag live in profiles (`color`, `negative`). B&W film = Gray + negative; on the transparency unit Gray falls back to the plain `Gray` mode, still 16-bit.
- Output is 16-bit TIFF or PNG, or 8-bit JPEG (`OutputFormat`, `write_image` picks the format from the extension; JPEG is written at quality `JPEG_QUALITY` = 95 and ignores the depth asked for, the Bit depth row then reads "8 bit (JPEG is always 8 bit)"). CLI: `--format png|jpg --negative --auto-levels --gray`.

## Auto-adjust (`scan-detect`)
- Holder and film shift between sessions, so preset frames are re-fitted: `fit_frames(img, window, presets, &Options)` moves each preset's rectangle outline (size unchanged) to where its four edges best match strong brightness edges (Sobel on blurred luma, gradients clipped at the 90th percentile of the strong ones, prefix sums so an edge costs O(1), absolute gradient so negatives and slides both work). A strip gets one shared shift (±`search_mm` = 3 mm) first, then each frame is refined ±`refine_mm` = 1 mm. No Hough: the outline match is simpler, has a confidence, and weak matches keep the preset.
- Confidence = peak prominence over the search window × edge strength relative to the image's mean gradient. Below `min_confidence` (0.25) a frame follows the strip's shift, or stays put when the strip match is weak too. Tunables are in `Options::default()`; tune from real film results.
- `Options::inset_mm` (GUI "Margin"): if the saved frames lie inside the visible frame boundary, set how far; the outline that is matched is the preset grown by it, the result is shrunk again. Without it a preset inset from the boundary has edges on both sides that pull in opposite directions.
- GUI: **Auto-adjust frames** fits the frames on the current overview (not saved unless "Save profile"); **Adjust frames before scan** runs `adjust_before_scan` per frame (600 dpi 8-bit pre-scan of the frame plus 5 mm margin, clamped to the source's `max_area`) inside `worker::run_batch` and reports each result in the status line. CLI: `scan --adjust`.
- Limits: no rotation correction, no size change. Strong structure near the frame (holder window, bed edges) attracts the match; the holder aperture is a legitimate target but check the result by eye the first times.

## Main window, strip and settings
- `apps/scan-gui` calls `ScanUi::show_main`: `TopBottomPanel::bottom` (strip, resizable), `SidePanel::left` (controls, resizable 240–560 px), `CentralPanel` (preview). eframe's `persistence` feature remembers window and panel sizes. The controls are collapsing sections in workflow order, top to bottom (Scanner & profile, Auto-adjust frames, Save profile, Image, Save to); Preview / Scan / Cancel with the progress bar, status line and file-name range are a bar **pinned at the bottom** of the panel (`TopBottomPanel::bottom(..).show_inside`; the floating window puts it under the scrolling settings). The frame controls (Add/Delete, the selector, rotate ⟲ ⟳) are one `horizontal_wrapped` line **above the preview** (`panels::frame_bar`) that only breaks when the pane is too narrow; the active frame's size is in the info line under the preview.
- **Image strip**: the images of the row chosen under "Save to", read from `latent.data.json` (`targets::row_images`, reloaded on target change, after each save and with ⟳). Tiles show 320 px thumbnails (click selects, double click opens the image editor, right-click: Open (editor) / Copy path). Thumbnails decode on 2 worker threads (`thumbs.rs`, `ImageReader::no_limits`; a 215 MB 16-bit scan takes ~0.3 s) and are cached in `<archive>/.thumbs/<size>/<hash>.png` with the **same key as latent** (hash of path, length, mtime), so both programs reuse them. After a scan the worker writes the 320 px cache thumbnail from the in-memory image, so the new tile appears at once. Merge note: extract a shared helper with latent's `images.rs`.
- **Settings** (`settings.rs`): the auto-adjust values (search range 0.5–8 mm, refine 0–3 mm, min. confidence 0–1, margin 0–3 mm) are sliders, remembered in `<config dir>/settings.toml` (`scan_profiles::config_dir`, written when the slider is released). Image sliders are not remembered: they reset with each new preview.
- **Image sliders** (`scan_pipeline::tone::Adjust`, part of `ToneOptions`, so they apply to the preview and to the saved scan; neutral = untouched, byte-identical): exposure ±2 EV, black point 0–40 %, white point 60–100 %, midtones (gamma) 0.5–2, warm/cool and tint (colour only). Applied after negative and auto levels. `Tone::apply` uses one 65536-entry lookup table per channel; `Tone::with_adjust` swaps only the adjustment. The preview re-renders live while a slider moves (the estimate comes from the active frame only, so it is cheap). CLI: `--exposure --black --white --midtones --warmth --tint`.

## Per-frame adjustments and Auto
- The image sliders belong to a frame: `ScanUi::adjusts` is a `Vec<Adjust>` kept the same length as `frames`. Selecting a frame shows and edits its values and re-renders the preview with them; "Add frame" copies the active frame's values, "Delete frame" removes its entry, choosing a profile or getting a new preview resets everything to neutral. `worker::Frame::adjust` carries each frame's values into `run_batch`, so every scan is saved with its own adjustments (applied after the request-wide negative setting). Not stored in profiles.
- **Auto** (`Adjust::auto_levels`) no longer hides a per-channel stretch: it measures the frame's pixels on the overview (0.1th and 99.8th percentile of all channels pooled, after the negative step when negative is on), limited to the slider ranges, and **writes black and white point into the frame's sliders**, so the saved scan equals what the sliders show and the result can be tweaked. Buttons: `Auto` (active frame), `Auto all` (each frame on its own area), `Reset` (active frame), `Copy to all`. The old "Exposure: auto" checkbox is gone from the UI; `ToneOptions::auto_levels` (per-channel) remains for the CLI (`--auto-levels`).

## Rotation
- One rotation (0/90/180/270, clockwise) turns the preview **and** the saved scans: `ScanUi::rotate`, buttons ⟲ ⟳ above the preview, `Rotate` stage in the worker. It is stored in profiles (`Profile::rotate`, default 0) and set when a profile is selected; the CLI `--rotate` defaults to the profile's.
- Frames stay in bed coordinates (normalized to the unrotated overview, mm via `crop_to_mm`). The preview texture is rotated in `preview::render` (`rotate_image`), and frames are edited in view space: `crop::to_view` / `from_view` convert before and after `crop::overlay`, and only a changed view is written back.

## Where scans are saved
- `ScanUi::standalone(archive_arg)` (the app) offers two destinations (`settings::SaveMode`): **Folder** (default `~/Pictures/latent-scan`, created on first save; plain files `<prefix>-<NN>.<ext>`, clashes get `-n` via `scan_archive::folder_target`) and **Latent archive** (browse for the archive root, accepted only with a `latent.data.json` inside, then pick table and row; files go to `images/<table>/<id>/` and into the row's `images` list). Choosing the archive radio with none chosen opens the browser at once. `ScanUi::new(archive)` (a host such as latent) is archive-only: no switch, no browse.
- Folder dialogs are `rfd` (xdg-desktop-portal backend on Linux, no GTK needed) on a thread (`worker::pick_folder`), answered as `Msg::Picked`.
- Remembered in `settings.toml` `[save]` (`mode`, `folder`, `archive`); an archive argument on the command line wins for that session. The film (table, row) is not remembered.
- `worker::Dest` (`Folder { dir }` | `Archive { root, table, row_id, write_data }`) replaces the old archive fields of `ScanRequest`; `Msg::Saved(SavedTo)` becomes `ScanEvent::Saved { table, row_id, rel }` (archive) or `ScanEvent::SavedFile { path }` (folder).
- The strip lists the folder's image files (oldest first) in folder mode, the row's images in archive mode. Thumbnail cache: `<archive>/.thumbs` (shared with latent) or `$XDG_CACHE_HOME/latent-scan/thumbs` for plain folders, so user folders stay clean.

## Form layout rule (`form.rs`)
- Every labelled control of the panels is a row of `form::form(ui, id, |f| ...)`: label column of fixed width (`LABEL_W`, longer labels are truncated, never wider) then the input filling the rest of the row. Rows: `combo` (dropdowns all as wide as the row), `slider` (no built-in text; track plus number box fill the row, `SliderOpts` for suffix/decimals/step/log/enabled), `text`, `number`, `check`, `value` (read-only, truncated, full text on hover), and `row` for anything custom (e.g. the scanner dropdown with its ⟳ button).
- Button rows use `form::button_row` (equal-width buttons filling the row). Do not add ad-hoc `Grid`s, `Slider::text` labels or fixed-width dropdowns; new controls go through `Form` so labels and inputs stay aligned.
- Buttons under a form (Save profile, Auto / Reset, Browse…, preset Save / Delete) use `form::form_buttons`: same equal-width row, but indented by `form::input_indent()` so it starts and ends where the inputs do (label column empty). Only the pinned Preview / Scan / Cancel bar and dialogs use the full-width `button_row`.
- **Double-click a slider to reset it**: pass the default with `SliderOpts::default_value(..)` (in the slider's own units, e.g. percent for black/white point); take it from the owning type (`Adjust::default()`, `DetectSettings::default()`, `Sharpen::default()`), never a literal. The hover text then says "Double-click: reset to ...". egui's `Slider` senses only drags, so `response.double_clicked()` is never true for it; `Form::slider` asks `pointer.button_double_clicked` while the pointer is over the slider (`form::reset_on_double_click`). Tests drive this with `form::test_support::Harness` (headless frames with simulated pointer events; let a grid settle for three frames before measuring row positions).
- egui's text field reports its *inner* rect (4 px frame margin outside); `form.rs` tests account for it.

## Zoom (`view.rs`)
- **Zoom to frame** (toggle in the frame bar) fills the preview pane with the active frame plus 25 % margin, using the overview's pixels (no extra scan, so it softens when zoomed far in). It follows the active frame and the rotation while the zoom was set by that button; editing a frame's edges does not move the view. Mouse wheel / pinch zooms at the pointer, middle or right button drag pans; either ends following. Zooming out to 1× returns to the fitted view (the picture centred in the pane, which keeps its size: `canvas::show` never shrinks the pane, so nothing around it moves while zooming); a new preview or a new set of frames resets it.
- Maths lives in `view.rs` (pure, tested): `ZoomState { center, zoom, follow }` in *view space* (normalized coordinates of the rotated picture); the visible `window` is derived from the pane's shape (`view::shape`), so resizing the pane never distorts. `crop::overlay` takes that window and maps pointer and frames through `to_screen` / `from_screen` / `screen_delta`; edits stay limited to the whole picture, not just the visible part. The picture is drawn with `uv = view::visible(window)`.

## Contrast, Darks, Whites
- `Adjust` (per frame, Image section) has `contrast`, `darks` and `whites`, each -1..1, neutral 0. They shape the tone curve *after* levels and gamma (`Adjust::map` -> `Adjust::shape`): darks and whites add `0.14 * slider *` a smooth bump that peaks at 1/3 (darks) or 2/3 (whites) and vanishes at black and white; contrast adds `slider * x(1-x)(2x-1)`, a cubic S-curve around mid-grey. Black and white map to themselves and the curve stays monotonic over the whole slider range (a grid test proves it). Difference to Black/White point: those clip and stretch the ends, Darks/Whites change how bright the shadows/highlights are without clipping. Auto still sets only black and white point. CLI: `--contrast --darks --whites`. The constants (0.14, cubic) are the first thing to tune from real film.

## Image editor (`editor.rs`) and sharpening
- Double-click a tile of the strip: an image view like latent's with a toolbar (◀ ▶ i/n, zoom out/fit/in, ⟲ ⟳, Crop, **Apply**, Revert, **Backup original**, **Restore original**), the picture on the left (wheel zoom, middle/right-drag pan, double-click = fit, crop overlay; `canvas.rs` and `view.rs` are shared with the scan preview) and a 300 px side column: RGB **histogram** (`histogram.rs`; grey for gray images; the two end bins are ignored for scaling), Film negative, the same image sliders as the scan (`panels::adjust_sliders`), **Sharpen** (Amount 0–300 %, Radius 0.3–5 px) and `Auto` / `Reset sliders`. Arrow keys step, Esc closes; stepping or closing drops unsaved edits and says so.
- **Preview first**: every change only affects the preview (a 2048 px 16-bit copy, decoded on a worker thread with `scan_pipeline::read_image`); the crop is an overlay until applied. `Edits { negative, adjust, sharpen, turns, crop }` is what is pending (`Edits::is_neutral`); rotation and crop are stored in the file's orientation and edited in view space (`crop::to_view/from_view`).
- **Apply** (worker thread, `editor::apply_file`): full-resolution decode -> `Crop` -> `Rotate` -> `ToneStage` -> `SharpenStage`, written atomically (hidden temp file with the same extension, then rename, `edit::overwrite_atomic`), same format, 16-bit kept; the strip thumbnail is rewritten, the editor reloads. **Apply never backs up silently**: with no backup it asks (`Back up and apply` / `Apply without backup` / `Cancel`). `Backup original` copies the file to `<root>/.originals/<relative path>` (latent's layout, `edit.rs`; only if none exists), `Restore original` copies it back and removes it, so latent's own Restore also works on these files. TIFF, PNG and JPEG are editable (a JPEG stays an 8-bit JPEG, re-compressed by every Apply, and the no-backup prompt says so); other files are view-only.
- **Sharpening** (`scan_pipeline::sharpen`): unsharp mask on brightness only (luma blurred with a separable Gaussian, sigma = radius; `amount * (luma - blur)` added to every channel, clamped), so colours do not fringe. It is spatial, so not part of `ToneOptions`; the preview scales the radius by preview/file size (`Sharpen::scaled`) and is therefore only approximate; judge fine detail after Apply (Restore original undoes it). Sharpening exists only in the editor, not at scan time.

## Presets (`presets.rs`)
- Named slider sets: the `Adjust` values plus sharpen amount/radius (not Film negative, crop or rotation). Stored in `<config dir>/presets.toml` (`[[preset]]`, written atomically); loaded values are clamped to the slider ranges, a broken file gives none plus a warning.
- Built-in **"Default"** (all neutral, first in the list; loading it resets sliders and sharpening) and **"SilverFast look (FP4)"** (exposure +0.10, contrast +0.20, darks -0.20, whites -1.00, midtones 0.83, sharpen 0.5/1.0), fitted to film 71 against its SilverFast scans; a user preset of the same name overrides it, deleting that restores the built-in. Built-ins cannot be deleted.
- UI: `panels::preset_row` (dropdown, name field, Save / Delete preset) sits above the sliders in the scan panel's Image section (loads into the active frame; sharpening ignored, an existing preset's sharpening is kept when saving there) and in the editor's side column (loads sliders and sharpening into the preview; Apply stays explicit). `ScanUi::presets` (`PresetUi`) is shared by both. No CLI option (`scan-cli` does not depend on `scan-ui`).

## Dust removal (`scan-pipeline/src/heal.rs`, `scan-ui/src/dust.rs`)
- Port of latent's dust tool (`heal` in latent's `images.rs`; keep both in sync) to 16-bit gray/RGB `RawImage`: each `Spot { x, y, r }` (fractions of the file's width/height, radius as a fraction of the width) is filled by copying the nearby texture that best continues its surroundings (grain included), feathered at the edge, flat median fill as fallback. `HealStage` is the `Stage`.
- In the image editor it is a pending edit like the others: **Dust** toggle next to Crop (the two modes exclude each other), brush **Size**, **Undo**, **Clear (n)**; click or drag on the picture marks spots (left button; wheel zoom and middle/right pan still work). `Edits::spots` holds them in the *file's* orientation (`dust::spot_from_view` / `spot_to_view` convert for the turned picture), so `Edits` is `Clone`, not `Copy`.
- Preview: `EditorState::refresh` heals the 2048 px base only when the spot list changed (cached in `healed`) and waits while the mouse is down (`painting`). **Apply** (`apply_file`) heals first, on the untouched full-resolution file, then crop -> rotate -> tone -> sharpen; 16 bits are kept (latent's is 8-bit). Backup/Restore original work as for the other edits.
- Not done: scan-time removal, the scanner's infrared dust channel, automatic dust detection.

## Output bit depth
- The scan is always taken at the profile's depth (the transparency unit delivers 16 bit anyway); the **file** depth is separate: `scan_pipeline::BitDepth` (`Sixteen` default, `Eight`), `write_image_depth` (`write_image` = 16 bit), `to_8bit` rounds `v * 255 / 65535`. The conversion is the very last step, after crop/rotate/tone/sharpen, so the finished look is quantised once (no dithering).
- GUI: **Bit depth** combo next to Format (`ScanUi::depth` -> `ScanRequest::depth` -> `worker::save_frame`); remembered in `settings.toml` `[save] eight_bit`. CLI: `scan --depth 8|16`.
- Editor: **Save as 8 bit** (`Edits::eight_bit`, a pending edit) halves a 16-bit file when applied; Restore original brings the 16 bit back. An 8-bit file (`scan_pipeline::file_depth` reads the header) stays 8 bit on every Apply, so editing never silently doubles it (the box is shown ticked and disabled).

## Preview resolution
- The overview is scanned at the **Preview** dropdown's resolution (Scanner & profile, under Resolution; the source's own values between 150 and 1200 dpi, default the lowest of at least 150, i.e. 300 on the 9000F transparency unit; remembered as `preview_dpi` in `settings.toml`). `ScanUi::preview_dpi_for` rounds up to a value the source offers. The whole window at 600 dpi is ~5100 x 5900 px (~90 MB 8-bit), at 1200 dpi ~360 MB while scanning.
- The stored 16-bit overview is shrunk with `preview::downscale` (block **average**, not nearest pixel) to at most `worker::preview_max_side(dpi)`: 2048 px up to 300 dpi (as before), 4096 above, so a higher preview dpi really gives more pixels (600 dpi -> 2960 px) while slider drags, which re-render the whole overview, stay affordable. The editor's preview uses the same `downscale`.
