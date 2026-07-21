/* 旅行足迹地图 Service Worker：瓦片/照片本地缓存 + app shell 预缓存，弱网/离线可用 */
const SHELL_CACHE = 'tm-shell-v2';  // app shell（HTML + maplibre 自托管资源 + manifest/icons）
const TILE_CACHE = 'tm-tiles-v2';   // v2: 清洗曾被永久缓存的 0 字节坏瓦片（OFM planet 版本轮换瞬间会返回 200+空体）
const PHOTO_CACHE = 'tm-photos-v1'; // 照片与缩略图（缓存优先）
const META_CACHE = 'tm-meta-v1';    // style.json / TileJSON（网络优先，断网兜底）
const MAX_TILES = 6000;             // 约 300-400MB 上限，超出按先入先出修剪
let putCount = 0;

// 离线打开必需的最小骨架：不预缓存这些，离线打开已安装的 PWA = 白屏
const SHELL_ASSETS = [
  '/', '/index.html',
  '/vendor/maplibre-gl.js', '/vendor/maplibre-gl.css',
  '/manifest.webmanifest', '/icons/icon-192.png', '/icons/icon-180.png',
];

self.addEventListener('install', e => {
  e.waitUntil(
    caches.open(SHELL_CACHE)
      .then(c => Promise.allSettled(SHELL_ASSETS.map(u => c.add(u)))) // 单个失败不阻塞整体安装
      .then(() => self.skipWaiting())
  );
});
const KEEP_CACHES = [SHELL_CACHE, TILE_CACHE, PHOTO_CACHE, META_CACHE];
self.addEventListener('activate', e => e.waitUntil(
  caches.keys()
    .then(keys => Promise.all(keys.filter(k => k.startsWith('tm-') && !KEEP_CACHES.includes(k)).map(k => caches.delete(k)))) // 清一切旧版缓存
    .then(() => self.clients.claim())
));

// 导航请求（HTML）：网络优先、失败回落缓存 app shell —— 离线/弱网仍能打开外壳，再由 JS 拉数据
async function shellFirst(req) {
  const cache = await caches.open(SHELL_CACHE);
  try {
    const resp = await fetch(req);
    if (resp && resp.ok) cache.put('/index.html', resp.clone());
    return resp;
  } catch (e) {
    return (await cache.match('/index.html')) || (await cache.match('/')) || Response.error();
  }
}

const isCdnTile = u => u.hostname.endsWith('basemaps.cartocdn.com') || u.hostname.endsWith('global.ssl.fastly.net');
const isTile = u => (u.pathname.startsWith('/ofm/') || u.pathname.startsWith('/carto/')) && /\.(pbf|png|jpg|jpeg|webp)$/.test(u.pathname);
const isPhoto = u => u.pathname.startsWith('/photos/');
const isGeo = u => u.pathname.startsWith('/geo/') && u.pathname.endsWith('.geojson'); // 国界 GeoJSON：本地打包，cache-first 免重下
const isMeta = u => u.pathname === '/tiles/style.json' || (u.pathname.startsWith('/ofm/') && !/\.[a-z]+$/i.test(u.pathname));

async function cacheFirst(cacheName, req) {
  const cache = await caches.open(cacheName);
  const hit = await cache.match(req);
  if (hit) return hit;
  const resp = await fetch(req);
  // 空体响应不入缓存：OFM 版本轮换瞬间会对旧路径返回 200+0B，一旦缓存该区域将永久空白
  if (resp.ok && resp.headers.get('content-length') !== '0') {
    cache.put(req, resp.clone());
    if (cacheName === TILE_CACHE && ++putCount % 200 === 0) trimTiles(cache);
  }
  return resp;
}

async function trimTiles(cache) {
  const keys = await cache.keys();
  if (keys.length > MAX_TILES) {
    for (const k of keys.slice(0, keys.length - MAX_TILES)) await cache.delete(k);
  }
}

async function networkFirst(req) {
  const cache = await caches.open(META_CACHE);
  try {
    const ctrl = new AbortController();
    const timer = setTimeout(() => ctrl.abort(), 6000);
    const resp = await fetch(req, { signal: ctrl.signal });
    clearTimeout(timer);
    if (resp.ok) cache.put(req, resp.clone());
    return resp;
  } catch (e) {
    const hit = await cache.match(req);
    if (hit) return hit;
    throw e;
  }
}

const isShell = u => u.pathname === '/vendor/maplibre-gl.js' || u.pathname === '/vendor/maplibre-gl.css'
  || u.pathname === '/manifest.webmanifest' || u.pathname.startsWith('/icons/');

self.addEventListener('fetch', e => {
  if (e.request.method !== 'GET') return;
  const u = new URL(e.request.url);
  if (u.origin !== location.origin) {
    if (isCdnTile(u)) e.respondWith(cacheFirst(TILE_CACHE, e.request)); // CDN 瓦片也本地缓存
    return;
  }
  if (u.pathname.startsWith('/api/')) return; // API 永不缓存
  // 导航请求（打开页面/PWA 冷启动）：网络优先 + 缓存 shell 兜底，杜绝离线白屏
  if (e.request.mode === 'navigate') { e.respondWith(shellFirst(e.request)); return; }
  if (isTile(u)) e.respondWith(cacheFirst(TILE_CACHE, e.request));
  else if (isGeo(u)) e.respondWith(cacheFirst(META_CACHE, e.request));
  else if (isPhoto(u)) e.respondWith(cacheFirst(PHOTO_CACHE, e.request));
  else if (isShell(u)) e.respondWith(cacheFirst(SHELL_CACHE, e.request)); // maplibre/icon 自托管资源缓存优先
  else if (isMeta(u)) e.respondWith(networkFirst(e.request));
});
