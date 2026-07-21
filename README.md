# 旅行足迹地图 travel-map-app

自托管的个人旅行地图：上传带 GPS 的照片自动落点、按城市/旅程整理、生成可分享的沉浸式旅行故事。单二进制部署。

线上：https://atlas.sol42.cn

## 技术栈

- **后端** `src/main.rs`：Rust + axum。静态托管前端、places/trips/照片 REST API、会话认证（token 只存哈希）、矢量瓦片代理（磁盘缓存）、备份导入导出。
- **前端** `public/index.html`：单文件 SPA（HTML+CSS+JS 全内联）+ MapLibre GL。地图足迹、旅程分组、护照名片、攻略视角、**地图故事模式**（翻页=镜头沿路线飞行）。
- **PWA** `public/sw.js`：瓦片/照片离线缓存，manifest + icons。
- **数据** `data/`（不入库）：`store.json`（地点/旅程）、`photos/`、`tilecache/`。

## 本地运行

```bash
cargo run --release          # 默认监听 0.0.0.0:8848，前端在 public/
```

前置：Rust stable、`public/` 与可写的 `data/` 目录。生产环境用 nginx 反代 + systemd。

## 环境变量

| 变量 | 默认 | 说明 |
|------|------|------|
| `ADMIN_PASSWORD` | 空 | 站主登录密码；未设则无法登录（只读展示） |
| `SITE_ORIGIN` | `https://atlas.sol42.cn` | 分享页 OG 图的绝对地址前缀 |
| `DEMO_SEED` | 关 | `=1` 时空库回退到内置样例数据（仅演示用） |
| `TILE_CACHE_MAX_MB` | `2048` | 瓦片磁盘缓存上限，超出按最旧修剪 |
| `IMPORT_MAX_MB` | `512` | 备份包解压总量上限（防 OOM） |

## 安全说明

- 站主写接口一律校验会话；访客只读，坐标与日期做服务端脱敏。
- 分享页 id 走白名单 + JSON 注入，防反射型 XSS。
- 限流身份取 nginx 的 `X-Real-IP`（不可伪造）。
- 瓦片代理有磁盘上限；备份导入有条目/单文件/总量上限与路径穿越校验。

## 部署

VPS 上 `cargo build --release`，产物 `target/release/travel-map-app`，由 systemd 拉起（`travel-map.service`），nginx 反代到本地端口。
