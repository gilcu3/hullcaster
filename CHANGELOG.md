# Changelog

## v0.3.0 (2026/04/06)

- Migrated to fully async architecture (tokio + reqwest)
- Periodic background feed synchronization
- Better AntennaPod compatibility
- Sync progress in notification bar no longer conflicts with other notifications
- Proper media-control support on macOS
- List position preserved when underlying data changes
- Lockfile support to prevent concurrent instances
- Database and subscription sync performance improvements
- Updated to Rust 2024 edition, rodio 0.22, rustls
- Comprehensive test suite covering db, gpodder, feeds, config, keymap, and utils

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
