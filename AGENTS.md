For a release, bump the patch version in both `Cargo.toml` and the `turmy` package entry in `Cargo.lock`.
Run formatting, Clippy with warnings denied, and the locked test suite; commit as `chore: release vX.Y.Z` and push `main`.
Push `main` to the `publish` branch, then monitor the Release workflow and verify the GitHub release assets and checksums.
