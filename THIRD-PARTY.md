# Third-party notices

GravityNote's code uses the [MIT license](LICENSE). The components below retain
their own licenses.

## GPUI

The app uses the unmodified GPUI 0.2.2 crate by Zed Industries, licensed under
Apache-2.0. Cargo downloads it from crates.io.

- [Upstream package](https://crates.io/crates/gpui/0.2.2)
- [License](licenses/GPUI-APACHE-2.0.txt)

## Lilex fonts

The bundled Lilex Regular, SemiBold, Bold, Italic, and BoldItalic fonts are by the Lilex Project Authors
and use the SIL Open Font License 1.1.

- [Upstream project](https://github.com/mishamyrt/Lilex)
- [License and copyright notices](assets/fonts/OFL.txt)

## Images

See [image licenses](IMAGE-LICENSES.md) for application artwork and screenshots.

## Other dependencies

Direct dependencies are listed in `Cargo.toml`; `Cargo.lock` records the resolved
versions. Run `cargo tree` to inspect the dependency graph. Consult each package's
license and notices when redistributing it.
