# Third-party notices

GravityNote's code uses the [MIT license](LICENSE). The components below retain
their own licenses.

## GPUI

`vendor/gpui/` contains a modified copy of GPUI 0.2.2 by Zed Industries,
licensed under Apache-2.0.

- [Upstream package](https://crates.io/crates/gpui/0.2.2)
- [License](vendor/gpui/LICENSE-APACHE)
- [Modifications](vendor/gpui/MODIFICATIONS.md)

The fork adds Liquid Glass rendering and window integration. Its source is
included in this repository so builds do not require a sibling checkout.

## Lilex fonts

The bundled Lilex Regular, SemiBold, Bold, Italic, and BoldItalic fonts are by the Lilex Project Authors
and use the SIL Open Font License 1.1.

- [Upstream project](https://github.com/mishamyrt/Lilex)
- [License and copyright notices](assets/fonts/OFL.txt)

## Images

See [image licenses](IMAGE-LICENSES.md) for application artwork, screenshots,
and the assets in the vendored GPUI examples.

## Other dependencies

Direct dependencies are listed in `Cargo.toml`; `Cargo.lock` records the resolved
versions. Run `cargo tree` to inspect the dependency graph. Consult each package's
license and notices when redistributing it.
