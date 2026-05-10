# Fuzz targets

Run a target with [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz):

```
cargo install cargo-fuzz
cd ..  # back to workspace root
cargo +nightly fuzz run protocol_frame -- -max_total_time=60
```

Targets are kept outside the main workspace so they can use nightly-only
features without affecting the rest of the build.

See `spec/16_benchmarks_acceptance/08_acceptance_test_suite.md` §18 for the
fuzzing strategy.
