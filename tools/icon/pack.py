"""Rasterise the icon's SVGs at their own sizes (and the artwork at 2x) with headless Edge, then pack tmr.ico: one PNG image a size,
32-bit with alpha, as Windows Vista and later read them. Usage: pack.py DIR, the directory dimm.py wrote: reads its SVGs, and writes
DIR/tmr.ico, DIR/icon<px>.png and DIR/dimm-burn@2x.png (and render.html, the page Edge draws them in)."""
import base64
import json
import os
import struct
import subprocess
import sys

EDGE = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"
SIZES = [256, 96, 80, 72, 64, 60, 48, 40, 36, 32, 30, 24, 20, 16]
if len(sys.argv) != 2:
    sys.exit("usage: pack.py DIR")
out_dir = sys.argv[1]


def data_url(name):
    return "data:image/svg+xml;base64," + base64.b64encode(open(os.path.join(out_dir, name), "rb").read()).decode()


jobs = [(f"icon{p}.svg", p, p, f"icon{p}.png") for p in SIZES] + [("dimm-burn.svg", 2691, 649, "dimm-burn@2x.png")]
script = ("const jobs=" + json.dumps([[data_url(src), w, h, name] for src, w, h, name in jobs]) + ";"
          "const out={};let left=jobs.length;"
          "for(const [src,w,h,name] of jobs){const img=new Image();"
          "img.onload=()=>{const c=document.createElement('canvas');c.width=w;c.height=h;"
          "c.getContext('2d').drawImage(img,0,0,w,h);out[name]=c.toDataURL('image/png');"
          "if(--left===0){document.getElementById('out').textContent=JSON.stringify(out);}};"
          "img.src=src;}")
page = os.path.abspath(os.path.join(out_dir, "render.html"))  # absolute, for its file:/// URL
open(page, "w", encoding="utf-8").write(f"<!doctype html><html><body><pre id='out'></pre><script>{script}</script></body></html>")
dom = subprocess.run([EDGE, "--headless=new", "--disable-gpu", "--virtual-time-budget=8000", "--dump-dom",
                      "file:///" + page.replace("\\", "/")], capture_output=True, text=True, encoding="utf-8").stdout
start = dom.index("<pre id=\"out\">") + len("<pre id=\"out\">")
raw = dom[start:dom.index("</pre>", start)].replace("&quot;", '"').replace("&amp;", "&")
images = {name: base64.b64decode(url.split(",", 1)[1]) for name, url in json.loads(raw).items()}
for name, png in images.items():
    open(os.path.join(out_dir, name), "wb").write(png)

# The .ico: ICONDIR, an ICONDIRENTRY a size (0 means 256), then the PNGs
entries, blobs = [], []
offset = 6 + 16 * len(SIZES)
for p in SIZES:
    png = images[f"icon{p}.png"]
    w, h = struct.unpack(">II", png[16:24])
    assert (w, h) == (p, p), (p, w, h)
    entries.append(struct.pack("<BBBBHHII", p % 256, p % 256, 0, 0, 1, 32, len(png), offset))
    blobs.append(png)
    offset += len(png)
ico = struct.pack("<HHH", 0, 1, len(SIZES)) + b"".join(entries) + b"".join(blobs)
open(os.path.join(out_dir, "tmr.ico"), "wb").write(ico)
print(f"tmr.ico: {len(SIZES)} sizes, {len(ico)} bytes; " + ", ".join(f"{p}px {len(images[f'icon{p}.png'])} B" for p in SIZES))
