# latent-scan

Scan film directly into a [latent](../latent) archive from a Canon CanoScan 9000F or Epson
Perfection V850 (via SANE). See `CLAUDE.md` for the architecture.

```
nix develop
cargo run -p scan-cli -- list
cargo run -p scan-gui -- ~/Latent        # preview, crop, scan, save
cargo run -p scan-cli -- scan -p 35mm-strip --archive ~/Latent --table roll_film --id 12 --name frame-01
```
