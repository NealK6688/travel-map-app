#!/usr/bin/env python3
"""瓦片预热：通过本机代理拉瓦片进磁盘缓存。
范围：全球 z0-6 + 日本及周边 z7-10 + 每个足迹点周边 z11-14。
用法：python3 prewarm_tiles.py [并发数，默认8]
"""
import json, math, sys, time, urllib.request
from concurrent.futures import ThreadPoolExecutor

BASE = "http://127.0.0.1:8848"
CONC = int(sys.argv[1]) if len(sys.argv) > 1 else 8


def get_json(path):
    with urllib.request.urlopen(BASE + path, timeout=30) as r:
        return json.load(r)


def t2xy(lat, lng, z):
    n = 2 ** z
    x = int((lng + 180) / 360 * n)
    lr = math.radians(lat)
    y = int((1 - math.log(math.tan(lr) + 1 / math.cos(lr)) / math.pi) / 2 * n)
    return max(0, min(n - 1, x)), max(0, min(n - 1, y))


def bbox(z, latmin, latmax, lngmin, lngmax):
    x0, y1 = t2xy(latmin, lngmin, z)
    x1, y0 = t2xy(latmax, lngmax, z)
    for x in range(min(x0, x1), max(x0, x1) + 1):
        for y in range(min(y0, y1), max(y0, y1) + 1):
            yield (z, x, y)


tiles = set()
for z in range(0, 7):  # 全球
    n = 2 ** z
    tiles.update((z, x, y) for x in range(n) for y in range(n))
for z in range(7, 11):  # 日本及周边
    tiles.update(bbox(z, 24, 46, 122, 146))
for p in get_json("/api/places")["places"]:  # 足迹点周边
    for z in range(11, 15):
        cx, cy = t2xy(p["lat"], p["lng"], z)
        r = 3 if z < 13 else 5
        n = 2 ** z
        for x in range(max(0, cx - r), min(n - 1, cx + r) + 1):
            for y in range(max(0, cy - r), min(n - 1, cy + r) + 1):
                tiles.add((z, x, y))

# 按当前底图模式选模板：本地代理才需要预热；直连外部 CDN 时无事可做
style = get_json("/tiles/style.json")
if "carto" in style.get("sources", {}):
    template = style["sources"]["carto"]["tiles"][0]
    if template.startswith("http"):
        print("[prewarm] 底图直连外部 CDN，无本地缓存可预热，跳过", flush=True)
        sys.exit(0)
else:
    template = get_json("/ofm/planet")["tiles"][0]
tiles = sorted(tiles)
total = len(tiles)
print(f"[prewarm] {total} tiles via {template}", flush=True)

done = [0, 0]  # ok, err


def fetch(t):
    z, x, y = t
    url = BASE + template.replace("{z}", str(z)).replace("{x}", str(x)).replace("{y}", str(y))
    for attempt in (1, 2):
        try:
            with urllib.request.urlopen(url, timeout=45) as r:
                r.read()
            done[0] += 1
            break
        except Exception:
            if attempt == 2:
                done[1] += 1
            else:
                time.sleep(1)
    n = done[0] + done[1]
    if n % 500 == 0:
        print(f"[prewarm] {n}/{total} ok={done[0]} err={done[1]}", flush=True)


t0 = time.time()
with ThreadPoolExecutor(max_workers=CONC) as ex:
    list(ex.map(fetch, tiles))
print(f"[prewarm] done: ok={done[0]} err={done[1]} in {time.time()-t0:.0f}s", flush=True)

# 清理旧版本瓦片缓存（template 形如 /ofm/planet/{version}/{z}/{x}/{y}.pbf）
import shutil, pathlib
parts = [s for s in template.split("/") if s]
if len(parts) >= 3 and parts[0] == "ofm" and parts[1] == "planet":
    current = parts[2]
    planet_dir = pathlib.Path(__file__).resolve().parent / "data" / "tilecache" / "planet"
    if planet_dir.is_dir():
        for d in planet_dir.iterdir():
            if d.is_dir() and d.name != current:
                shutil.rmtree(d, ignore_errors=True)
                print(f"[prewarm] pruned old version {d.name}", flush=True)
