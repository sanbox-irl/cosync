# Changelog

## [Unreleased]

- BREAKING: this library used to use `parking_lot` internally. Now by default we only use the `std`, but have added a feature flag for `parking_lot`.
- BREAKING: `is_executing` has been renamed `is_running`. It now takes a parameter. Use `is_running_any` for previous behavior.
- BREAKING: `SleepForTick` has been made private, and now can only be accessed via `sleep_for_tick`. Since `SleepForTick` was `[doc(hidden)]`, this should not break most code.
- BREAKING: `run_until_stall` has been renamed to `run` to denote its primacy.
- Added: `queue`ing a task returns a `CosyncTaskId`. This can be used in `unqueue_task`, `stop_running_task`, and for comparison operations between task ids.
- Added: an example for how to use Cosync.
- Removed: `CosyncWasDropped` was declared, but not used. This is a breaking change, but would be bizarre if a user relied on it.

## [0.2.1] - 2022-04-22

- Added: Allowed creating a queue handle from `CosyncInput`.
- Changed: Relaxed `clone`/`send`/`sync` restrictions on `CosyncInput`
- Changed: this changelog to be in current format.

## [0.2.0] - 2022-02-25

- Added: Allowed for unsized types to be used as the parameter of the cosync. This enables some wild dynamic dispatch code.
- Fixed banners and minor issues.

## [0.1.0] - 2022-02-06

- Initial implementation of the library.

[unreleased]: https://github.com/sanbox-irl/cosync/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/sanbox-irl/cosync/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/sanbox-irl/cosync/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/sanbox-irl/cosync/releases/tag/v0.1.0
