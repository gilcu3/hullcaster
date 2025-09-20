# Changelog

## v0.2.0 (2025/09/20)

- Major changes
  - Interface overhaul, now based on ratatui
  - Internal player based on rodio
  - mpris control support
  - reproducible builds using repro-env
  - CI testing and publishing for linux and macos
  - local dev support using cargo-make
- Some dependencies made the musl build impossible, might try to recover it in
  the future
- Minor bug fixes

## v0.1.2 (2024/11/02)

- Minor bug fixes
- Bundle sqlite to allow static builds using musl
- Check MIME type when downloading episode via #6

## v0.1.1 (2024/08/12)

- Minor bug fixes
- Added unplayed episodes view
- Added Nix package support via #1

## v0.1.0 (2024/07/02)

- Initial release
