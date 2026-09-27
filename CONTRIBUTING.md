# Contributing to GravityNote

Bug reports, documentation corrections, and pull requests are welcome. Include
steps to reproduce a bug, the expected result, and your macOS version. Remove
private notes, maps, and images from reports.

Discuss new dependencies, public API changes, and major features in an issue
before starting work.

## Development build

On macOS, with the [build requirements](README.md#build-and-run) installed:

```sh
./dev.sh
```

This builds and launches a development app with a DEV icon. Its default data
folder is `~/Library/Application Support/gravitynote-gpui-dev/`. Use sample documents
when testing file opening or changing the storage folder.

## Before submitting

For code changes, run the same build and tests as CI:

```sh
cargo build --release
cargo test --release
```

For documentation-only changes, check links and verify commands and feature
claims against the source. For UI changes, also check the affected behavior in
the development app; automated tests do not verify its appearance.

Keep changes focused and follow the surrounding code style. In the pull request,
explain the problem, the change, and what you checked. Include before-and-after
screenshots for visible changes.
