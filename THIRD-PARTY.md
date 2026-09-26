# Third-party notices

GravityNote itself is [MIT](LICENSE). Everything it bundles is listed here.

## gpui — Apache License 2.0

`vendor/gpui/` is a modified copy of [`gpui`](https://crates.io/crates/gpui)
0.2.2 by Zed Industries, used under the Apache License 2.0. The licence text is
at [`vendor/gpui/LICENSE-APACHE`](vendor/gpui/LICENSE-APACHE), and the changes
are listed in [`vendor/gpui/MODIFICATIONS.md`](vendor/gpui/MODIFICATIONS.md);
each changed file also says so at the top.

The fork adds a live Liquid Glass material. It is vendored rather than pulled
from crates.io so this repository builds on its own, with no sibling checkout.

No Zed editor source is present. Zed's editor is GPL-licensed and none of it is
copied, vendored or linked here — `gpui` is a separate, Apache-2.0 crate that
Zed publishes.

## Lilex — SIL Open Font License 1.1

`assets/fonts/Lilex-*.ttf` by the Lilex Project Authors, from
<https://github.com/mishamyrt/Lilex>. Licence text:
[`assets/fonts/OFL.txt`](assets/fonts/OFL.txt).

The OFL permits bundling in and selling an application. It forbids selling the
font on its own, and forbids shipping a modified version under the reserved
name "Lilex" — neither of which this does.

## Rust crates

Everything in `Cargo.toml` is MIT, Apache-2.0, or both. `cargo tree` lists the
full graph; `cargo license` or `cargo deny` will re-verify it.

## Fitness for the App Store

Nothing bundled here is copyleft. MIT, Apache-2.0 and the OFL are all
compatible with the App Store's distribution terms — the conflict people
remember is with the GPL, which is not used anywhere in this tree.

Apache-2.0 asks for three things when you ship a binary containing `gpui`:
include the licence, keep the attribution, and state that the files were
changed. This repository does all three, so a build of it can be shipped as-is.
