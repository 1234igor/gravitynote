# Image licenses

## Application artwork

`docs/screenshot.png` shows the application with sample content. The screenshot is
covered by the project's [MIT license](LICENSE), except for the app icon.

`assets/icons/*.svg` and `assets/icon/GravityNoteMenuBar.svg` are project UI artwork covered by [MIT](LICENSE).

The application and development icons in `assets/icon/` are project branding.
They are included for building this app. As stated in the [README](README.md#license),
the app name and icons are excluded from the code license; distributed forks
must use their own name and icons.

## Vendored GPUI examples

These fixtures are used by the optional examples in `vendor/gpui/`.

| Asset | Source | License |
| --- | --- | --- |
| `examples/image/app-icon.png` | Cropped and resized [Hyde Street Pier, San Francisco](https://commons.wikimedia.org/wiki/File:Hyde_Street_Pier._San_Francisco._(37699770496).jpg), Bernard Spragg | [CC0](https://creativecommons.org/publicdomain/zero/1.0/) |
| `examples/image/harbour-pan.gif` | Animated crops of the same photograph, copied from [GPUI Liquid Glass](https://github.com/1234igor/gpui-liquid-glass/blob/main/vendor/gpui/examples/image/make-demo-assets.py) | CC0 |
| `examples/image/arrow_circle.svg` | Zed Industries, GPUI 0.2.2 | [Apache-2.0](vendor/gpui/LICENSE-APACHE) |
| `examples/image/color.svg` | GPUI example with Lucide-derived markup | [Apache-2.0](vendor/gpui/LICENSE-APACHE) and [Lucide/Feather notices](vendor/gpui/LICENSE-LUCIDE) |
| `examples/svg/dragon.svg` | [Dragon clip art](https://commons.wikimedia.org/wiki/File:Dragon_clip_art.svg), Ebaychatter0; artwork unchanged | [CC BY-SA 3.0](https://creativecommons.org/licenses/by-sa/3.0/) |

Keep the dragon's author, source, and license with copies. Adaptations must use
the same or a compatible license. Keep the Lucide/Feather and Apache notices
with the SVG fixtures.

Remote-image examples fetch the credited harbour photograph from the GPUI
Liquid Glass repository. The uncredited cat GIF and logo fixture have been
replaced.
