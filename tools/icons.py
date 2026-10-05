#!/usr/bin/env python3
"""Generates the grainy "qb" icon set in web/icons/.

The letterforms are qBittorrent's logo glyphs (GPL-2.0-or-later, the
qBittorrent project). Grain is an SVG noise filter. Its frequency is scaled per
output size so the grain stays about 3 px at every size instead of blurring
away on small icons.

Rasterising needs an SVG renderer with filter support (ImageMagick's built-in
one has none). Pass any `render <in.svg> <out.png> <size>` command,
e.g. resvg:  tools/icons.py resvg
"""
import os, subprocess, sys

Q = "M317.273 639.45q51.227 0 74.68-27.466 23.453-27.464 24.996-92.578v-11.418q0-70.976-24.07-102.144-24.07-31.168-76.223-31.168-45.055 0-69.125 35.18-23.762 34.87-23.762 98.75 0 63.879 23.454 97.515 23.761 33.328 70.05 33.328zm-7.715 42.894q-65.421 0-102.144-45.98-36.723-45.981-36.723-128.376 0-83.011 37.032-129.609 37.03-46.598 103.07-46.598 69.433 0 106.773 52.461h2.778l7.406-46.289h40.426V828h-51.227V683.27q0-30.86 3.395-52.461h-4.012q-35.488 51.535-106.774 51.535z"
B = "M712.898 332.399q66.657 0 103.38 45.671 37.03 45.364 37.03 128.684t-37.34 129.61q-37.03 45.98-103.07 45.98-33.02 0-60.484-12.035-27.156-12.344-45.672-37.649h-3.703l-10.8 43.512h-36.724V196h51.227v116.65q0 39.191-2.469 70.359h2.47q35.796-50.61 106.155-50.61zm-7.406 42.894q-52.46 0-75.605 30.242-23.145 29.934-23.145 101.219t23.762 102.145q23.761 30.55 76.222 30.55 47.215 0 70.36-34.254 23.144-34.562 23.144-99.058 0-66.04-23.144-98.442-23.145-32.402-71.594-32.402z"

BLUE = '<linearGradient id="blue" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="#7cbcf8"/><stop offset="1" stop-color="#2f63b3"/></linearGradient>'


def grain(fid, size, strength, rough):
    """Noise multiplied into the shape, plus a small displacement to roughen the edges."""
    f = size / 3072  # ~3 px grain at any output size
    return f'''<filter id="{fid}" x="-5%" y="-5%" width="110%" height="110%" color-interpolation-filters="sRGB">
  <feTurbulence type="fractalNoise" baseFrequency="{f:.4f}" numOctaves="2" seed="7" result="noise"/>
  <feDisplacementMap in="SourceGraphic" in2="noise" scale="{rough}" xChannelSelector="R" yChannelSelector="G" result="rough"/>
  <feColorMatrix in="noise" type="saturate" values="0" result="mono"/>
  <feComponentTransfer in="mono" result="speck">
    <feFuncR type="linear" slope="{strength}" intercept="{1 - strength / 2:.3f}"/>
    <feFuncG type="linear" slope="{strength}" intercept="{1 - strength / 2:.3f}"/>
    <feFuncB type="linear" slope="{strength}" intercept="{1 - strength / 2:.3f}"/>
  </feComponentTransfer>
  <feBlend in="rough" in2="speck" mode="multiply" result="tex"/>
  <feComposite in="tex" in2="rough" operator="in"/>
</filter>'''


def letters(fill_q, fill_b, scale):
    return (f'<g transform="translate(512 512) scale({scale}) translate(-512 -512)">'
            f'<path d="{Q}" fill="{fill_q}"/><path d="{B}" fill="{fill_b}"/></g>')


def svg(body, defs=""):
    return f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 1024 1024"><defs>{defs}</defs>{body}</svg>\n'


def any_icon(size):
    """Just the letters, on transparent: browser tabs, desktop installs."""
    return svg(f'<g filter="url(#g)">{letters("url(#blue)", "url(#blue)", 1.28)}</g>',
               BLUE + grain("g", size, 2.8, 4))


def maskable_icon(size):
    """Full-bleed for Android launchers, which crop to their own shape;
    letters stay inside the 80% safe zone."""
    return svg(f'<rect width="1024" height="1024" fill="url(#blue)" filter="url(#bg)"/>'
               f'<g filter="url(#g)">{letters("#d6ecff", "#ffffff", 0.9)}</g>',
               BLUE + grain("bg", size, 1.8, 0) + grain("g", size, 1.4, 4))


def mono_icon(_size):
    """Android 13+ themed icons recolour the alpha mask, so no texture here."""
    return svg(letters("#fff", "#fff", 0.9))


def main():
    here = os.path.join(os.path.dirname(__file__), "..", "web", "icons")
    render = sys.argv[1:] or None
    outputs = [
        (any_icon, "icon-192.png", 192), (any_icon, "icon-512.png", 512),
        (maskable_icon, "maskable-192.png", 192), (maskable_icon, "maskable-512.png", 512),
        (maskable_icon, "apple-touch-icon.png", 180), (mono_icon, "monochrome-512.png", 512),
    ]
    # the page favicon / login logo is served as SVG
    with open(os.path.join(here, "icon.svg"), "w") as f:
        f.write(any_icon(512))
    if not render:
        print("wrote icon.svg; pass a renderer command to also write PNGs")
        return
    tmp = os.path.join(here, ".tmp.svg")
    for make, name, size in outputs:
        with open(tmp, "w") as f:
            f.write(make(size))
        subprocess.run([*render, tmp, os.path.join(here, name), str(size)], check=True)
    os.remove(tmp)
    print("wrote", ", ".join(n for _, n, _ in outputs))


if __name__ == "__main__":
    main()
