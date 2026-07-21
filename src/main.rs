use axum::{
    extract::{Multipart, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::{delete, get, patch, post},
    Json, Router,
};
use image::ExtendedColorType;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Cursor,
    path::{Path as FsPath, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;
use tower_http::services::ServeDir;

const OFM: &str = "https://tiles.openfreemap.org";
const SESSION_MAX_AGE: u64 = 2_592_000; // 30 天

fn ofm_style() -> String {
    std::env::var("OFM_STYLE").unwrap_or_else(|_| "liberty".to_string())
}
fn data_dir() -> PathBuf {
    PathBuf::from("data")
}
fn cache_dir() -> PathBuf {
    data_dir().join("tilecache")
}
// 瓦片磁盘缓存上限（默认 2GB，TILE_CACHE_MAX_MB 可覆盖）。瓦片代理无鉴权，
// 匿名请求成功即落盘，无上限则可被刷爆磁盘。每积累若干次写触发一次修剪，按 mtime 删最旧到降回上限。
static TILE_WRITE_COUNT: AtomicU64 = AtomicU64::new(0);
fn tile_cache_cap_bytes() -> u64 {
    std::env::var("TILE_CACHE_MAX_MB").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(2048) * 1024 * 1024
}
fn collect_cache_files(dir: &FsPath, out: &mut Vec<(PathBuf, u64, std::time::SystemTime)>, total: &mut u64) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for ent in rd.flatten() {
        let path = ent.path();
        let Ok(md) = ent.metadata() else { continue };
        if md.is_dir() {
            collect_cache_files(&path, out, total);
        } else {
            let sz = md.len();
            *total += sz;
            let mt = md.modified().unwrap_or(std::time::UNIX_EPOCH);
            out.push((path, sz, mt));
        }
    }
}
fn maybe_prune_tile_cache() {
    // 每 500 次写检查一次，避免每块瓦片都全盘扫描
    if TILE_WRITE_COUNT.fetch_add(1, Ordering::Relaxed) % 500 != 0 {
        return;
    }
    tokio::task::spawn_blocking(|| {
        let cap = tile_cache_cap_bytes();
        let dir = cache_dir();
        let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
        let mut total: u64 = 0;
        collect_cache_files(&dir, &mut files, &mut total);
        if total <= cap {
            return;
        }
        files.sort_by_key(|(_, _, mt)| *mt); // 最旧在前
        let mut freed: u64 = 0;
        for (path, sz, _) in files {
            if total.saturating_sub(freed) <= cap {
                break;
            }
            if fs::remove_file(&path).is_ok() {
                freed += sz;
            }
        }
        eprintln!("[tilecache] 修剪完成：释放 {} MB，当前约 {} MB", freed / 1_048_576, total.saturating_sub(freed) / 1_048_576);
    });
}
fn photo_dir() -> PathBuf {
    data_dir().join("photos")
}
fn thumb_dir() -> PathBuf {
    photo_dir().join("thumb")
}
fn store_file() -> PathBuf {
    data_dir().join("store.json")
}
fn sessions_file() -> PathBuf {
    data_dir().join("sessions.json")
}

// ---------- 数据模型 (v3) ----------
#[derive(Clone, Serialize, Deserialize)]
struct Visit {
    id: String,
    #[serde(default)]
    date: String, // 可空字符串
    #[serde(default)]
    note: String,
    #[serde(default)]
    rating: u8,
}

#[derive(Clone, Serialize, Deserialize)]
struct ChecklistItem {
    id: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    done: bool,
}

// 访客留言（公开可见；站主可删）
#[derive(Clone, Serialize, Deserialize)]
struct GuestMessage {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    ts: u64, // unix 秒
    #[serde(default, rename = "placeId")]
    place_id: Option<String>, // 关联地点（可空=留在整站）
}

#[derive(Clone, Serialize, Deserialize)]
struct UnplacedPhoto {
    id: String,
    url: String,
    thumb: String,
    #[serde(default)]
    name: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct Photo {
    id: String,
    url: String,
    thumb: String,
    #[serde(default)]
    date: Option<String>,
}

fn default_place_status() -> String {
    "visited".to_string()
}
fn default_trip_status() -> String {
    "planned".to_string()
}

#[derive(Clone, Serialize, Deserialize)]
struct Place {
    id: String,
    lng: f64,
    lat: f64,
    name: String,
    short: String,
    #[serde(default)]
    city: String,
    #[serde(default)]
    country: String,
    #[serde(default)]
    em: String,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    rating: u8,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    feel: String,
    #[serde(default)]
    photos: Vec<Photo>,
    #[serde(default)]
    cover: Option<String>,
    #[serde(default = "default_place_status")]
    status: String, // "visited" | "planned"
    #[serde(default, rename = "tripId")]
    trip_id: Option<String>,
    #[serde(default)]
    visits: Vec<Visit>,
    #[serde(default)]
    category: String, // "景点"|"美食"|"自然"|"城市"|"住宿"|"购物"|"其他"|"" (空=未分类)
    #[serde(default)]
    color: Option<String>, // 情绪色卡：封面照片主色（hex），前端给标记染色
    #[serde(default, skip_serializing_if = "Value::is_null")]
    guide: Value, // 攻略字段（实操型）：{stay,cost,when,tip,transport}，前端定义 shape，后端透明存取
}

// 地点类别白名单（空串=未分类）
const CATEGORY_WHITELIST: &[&str] = &["景点", "美食", "自然", "城市", "住宿", "购物", "其他", ""];
fn is_valid_category(c: &str) -> bool {
    CATEGORY_WHITELIST.contains(&c)
}

#[derive(Clone, Serialize, Deserialize)]
struct Trip {
    id: String,
    name: String,
    #[serde(default)]
    em: String,
    #[serde(default)]
    color: String,
    #[serde(default, rename = "startDate")]
    start_date: String,
    #[serde(default, rename = "endDate")]
    end_date: String,
    #[serde(default)]
    note: String,
    #[serde(default = "default_trip_status")]
    status: String, // "done" | "planned"
    #[serde(default)]
    checklist: Vec<ChecklistItem>,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    guide: Value, // 攻略字段：{season,days,transport,budget:[{k,v,c}]}，前端定义 shape
}

#[derive(Clone, Serialize, Deserialize)]
struct City {
    nm: String,
    lng: f64,
    lat: f64,
}

fn default_version() -> u32 {
    4
}

#[derive(Clone, Serialize, Deserialize)]
struct Store {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    places: Vec<Place>,
    #[serde(default)]
    trips: Vec<Trip>,
    #[serde(default)]
    cities: Vec<City>,
    #[serde(default)]
    unplaced: Vec<UnplacedPhoto>,
    #[serde(default)]
    profile: Profile,
    // v5 互动/统计（老 store 无这些字段，serde default 兼容）
    #[serde(default)]
    likes: std::collections::HashMap<String, u32>, // place_id → ❤️ 数
    #[serde(default)]
    messages: Vec<GuestMessage>,
    #[serde(default)]
    views: u64, // 全站访问计数（不含站主自己）
}
impl Default for Store {
    fn default() -> Self {
        Store {
            version: 4,
            places: vec![],
            trips: vec![],
            cities: vec![],
            unplaced: vec![],
            profile: Profile::default(),
            likes: std::collections::HashMap::new(),
            messages: vec![],
            views: 0,
        }
    }
}

// 站主名片：访客据此知道「这是谁的地图」；空 name 时前端回落「我的旅行足迹」
#[derive(Clone, Serialize, Deserialize, Default)]
struct Profile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    bio: String,
    #[serde(default)]
    about: String, // 「关于」页的长介绍（可空）
}

#[derive(Clone, Serialize, Deserialize)]
struct SessionEntry {
    hash: String, // token 的 sha256 hex
    exp: u64,     // unix 秒
}

#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<Store>>,
    sessions: Arc<std::sync::Mutex<Vec<SessionEntry>>>,
    http: reqwest::Client,
    // 登录失败节流：IP → (窗口内失败次数, 锁定截止 unix 秒)
    login_guard: Arc<std::sync::Mutex<std::collections::HashMap<String, (u32, u64)>>>,
    // 点赞去重：已点过的 "ip|place_id" 集合（进程内，重启清零，配合前端 localStorage）
    like_guard: Arc<std::sync::Mutex<std::collections::HashSet<String>>>,
    // 留言限流：IP → 上次留言 unix 秒
    msg_guard: Arc<std::sync::Mutex<std::collections::HashMap<String, u64>>>,
}

// ---------- 持久化（原子写盘 + fsync） ----------
// 写临时文件 → sync_all（保证数据块落盘，rename 原子性才有意义）→ rename → fsync 父目录。
// 返回 io::Result：失败必须能被调用方感知，绝不静默吞（磁盘满/只读挂载时旧代码会假成功）。
fn atomic_write(path: &FsPath, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?; // 关键：确保临时文件内容真正落盘，防掉电后 rename 出截断文件
    }
    fs::rename(&tmp, path)?;
    // 父目录 fsync：确保 rename 这条目录项本身持久化（best-effort，失败不致命）
    if let Some(dir) = path.parent() {
        if let Ok(d) = fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
    Ok(())
}
fn load_store() -> Store {
    match fs::read_to_string(store_file()) {
        Ok(t) => match serde_json::from_str::<Store>(&t) {
            Ok(s) => s,
            Err(e) => {
                // 解析失败绝不能静默当空库——那会让磁盘上完好的元数据在应用里"消失"，
                // 且随后任意写操作会用空库覆盖掉原文件。先把损坏文件另存留证，再退空库启动。
                let bak = data_dir().join(format!("store.corrupt.{}.json", now_secs()));
                let _ = fs::copy(store_file(), &bak);
                eprintln!(
                    "[FATAL] store.json 解析失败：{e}；已备份损坏文件到 {bak:?}，以空库启动。请人工检查后恢复！"
                );
                Store::default()
            }
        },
        Err(_) => Store::default(), // 文件不存在：全新部署，空库正常
    }
}
fn save_store(s: &Store) {
    match serde_json::to_string_pretty(s) {
        Ok(t) => {
            if let Err(e) = atomic_write(&store_file(), &t) {
                eprintln!("[ERROR] save_store 写盘失败：{e}（数据未持久化，重启将丢失本次改动！）");
            }
        }
        Err(e) => eprintln!("[ERROR] save_store 序列化失败：{e}"),
    }
}

// 启动迁移：v1（无 version 字段）→ v2 → v3 → v4，链式、幂等，每步迁移前按当前版本备份
fn migrate_store() {
    let path = store_file();
    let Ok(text) = fs::read_to_string(&path) else { return };
    let Ok(mut v) = serde_json::from_str::<Value>(&text) else { return };
    let ver = v.get("version").and_then(|x| x.as_u64()).unwrap_or(1);
    if ver >= 4 {
        return; // 已是 v4，跳过
    }

    // v1 → v3 段：仍走原逻辑，迁移前备份 store.v1.bak.json / store.v2.bak.json
    if ver < 3 {
        let bak = if ver == 1 { "store.v1.bak.json" } else { "store.v2.bak.json" };
        let _ = fs::copy(&path, data_dir().join(bak));
        if ver == 1 {
            // v1 → v2
            if let Some(places) = v.get_mut("places").and_then(|p| p.as_array_mut()) {
                for p in places {
                    if let Some(obj) = p.as_object_mut() {
                        obj.entry("status").or_insert(json!("visited"));
                        obj.entry("tripId").or_insert(Value::Null);
                        obj.entry("cover").or_insert(Value::Null);
                    }
                }
            }
            if let Some(obj) = v.as_object_mut() {
                obj.entry("trips").or_insert(json!([]));
            }
        }
        // v2 → v3：places 补 visits（有 date 的生成一条 visit），trips 补 checklist，store 补 unplaced
        if let Some(places) = v.get_mut("places").and_then(|p| p.as_array_mut()) {
            for (i, p) in places.iter_mut().enumerate() {
                if let Some(obj) = p.as_object_mut() {
                    if !obj.contains_key("visits") {
                        let date = obj
                            .get("date")
                            .and_then(|d| d.as_str())
                            .unwrap_or("")
                            .to_string();
                        let visits = if date.is_empty() {
                            json!([])
                        } else {
                            json!([{ "id": format!("v_{}_{}", uid(), i), "date": date, "note": "" }])
                        };
                        obj.insert("visits".into(), visits);
                    }
                }
            }
        }
        if let Some(trips) = v.get_mut("trips").and_then(|t| t.as_array_mut()) {
            for t in trips {
                if let Some(obj) = t.as_object_mut() {
                    obj.entry("checklist").or_insert(json!([]));
                }
            }
        }
        if let Some(obj) = v.as_object_mut() {
            obj.entry("unplaced").or_insert(json!([]));
            obj.insert("version".into(), json!(3));
        }
    }

    // v3 → v4：每个 place 补 category:""（幂等，已存在则不覆盖），迁移前备份 store.v3.bak.json
    let cur = v.get("version").and_then(|x| x.as_u64()).unwrap_or(3);
    if cur == 3 {
        let _ = fs::copy(&path, data_dir().join("store.v3.bak.json"));
        if let Some(places) = v.get_mut("places").and_then(|p| p.as_array_mut()) {
            for p in places {
                if let Some(obj) = p.as_object_mut() {
                    obj.entry("category").or_insert(json!(""));
                }
            }
        }
        if let Some(obj) = v.as_object_mut() {
            obj.insert("version".into(), json!(4));
        }
    }

    if let Ok(t) = serde_json::to_string_pretty(&v) {
        let _ = atomic_write(&path, &t);
    }
}

fn load_sessions() -> Vec<SessionEntry> {
    let now = now_secs();
    if let Ok(t) = fs::read_to_string(sessions_file()) {
        if let Ok(mut v) = serde_json::from_str::<Vec<SessionEntry>>(&t) {
            v.retain(|e| e.exp > now);
            return v;
        }
    }
    vec![]
}
fn save_sessions(list: &[SessionEntry]) {
    if let Ok(t) = serde_json::to_string(list) {
        if let Err(e) = atomic_write(&sessions_file(), &t) {
            eprintln!("[ERROR] save_sessions 写盘失败：{e}");
        }
    }
}

// ---------- 工具 ----------
fn uid() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{:x}", n)
}
fn now_secs() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}
fn sha256_hex(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    h.finalize().iter().map(|x| format!("{:02x}", x)).collect()
}
fn new_token() -> String {
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{:02x}", b)).collect()
}
fn cookie_token(headers: &HeaderMap) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix("session=") {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}
// 客户端 IP（限流身份）。
// 优先 X-Real-IP：nginx 用 `proxy_set_header X-Real-IP $remote_addr` 写入真实对端，客户端无法伪造（nginx 覆盖）。
// X-Forwarded-For 的“第一跳”是客户端可自填的，绝不能作为主来源（会让限流被绕过或错误全局共享）。
// XFF 仅作兜底，且取“最后一跳”——即 nginx 追加的真实对端，而非客户端伪造的头部。
fn client_ip(headers: &HeaderMap) -> String {
    if let Some(ip) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        let ip = ip.trim();
        if !ip.is_empty() {
            return ip.to_string();
        }
    }
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(last) = xff.split(',').filter(|s| !s.trim().is_empty()).last() {
            return last.trim().to_string();
        }
    }
    "unknown".to_string()
}
fn is_owner(st: &AppState, headers: &HeaderMap) -> bool {
    let Some(tok) = cookie_token(headers) else { return false };
    let h = sha256_hex(tok.as_bytes());
    let now = now_secs();
    st.sessions.lock().unwrap().iter().any(|e| e.hash == h && e.exp > now)
}
fn unauthorized() -> axum::response::Response {
    (StatusCode::UNAUTHORIZED, Json(json!({"ok": false, "error": "unauthorized"}))).into_response()
}

fn chinese_text_field() -> Value {
    json!([
        "coalesce",
        ["get", "name:zh-Hans"],
        ["get", "name:zh"],
        ["get", "name:zh-Hant"],
        ["get", "name:latin"],
        ["get", "name"]
    ])
}

fn content_type_for(p: &str) -> &'static str {
    if p.ends_with(".pbf") {
        "application/x-protobuf"
    } else if p.ends_with(".png") {
        "image/png"
    } else if p.ends_with(".jpg") || p.ends_with(".jpeg") {
        "image/jpeg"
    } else if p.ends_with(".webp") {
        "image/webp"
    } else if p.ends_with(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

fn delete_photo_files(photo_id: &str) {
    let _ = fs::remove_file(photo_dir().join(format!("{photo_id}.jpg")));
    let _ = fs::remove_file(thumb_dir().join(format!("{photo_id}.jpg")));
}

// ---------- EXIF ----------
fn exif_of(bytes: &[u8]) -> Option<exif::Exif> {
    exif::Reader::new()
        .read_from_container(&mut Cursor::new(bytes))
        .ok()
}
fn gps_coord(ex: &exif::Exif, coord: exif::Tag, refr: exif::Tag) -> Option<f64> {
    let f = ex.get_field(coord, exif::In::PRIMARY)?;
    let dms = match &f.value {
        exif::Value::Rational(v) if v.len() >= 3 => v,
        _ => return None,
    };
    let mut deg = dms[0].to_f64() + dms[1].to_f64() / 60.0 + dms[2].to_f64() / 3600.0;
    if let Some(rf) = ex.get_field(refr, exif::In::PRIMARY) {
        if let exif::Value::Ascii(a) = &rf.value {
            if let Some(first) = a.first() {
                if first.first() == Some(&b'S') || first.first() == Some(&b'W') {
                    deg = -deg;
                }
            }
        }
    }
    Some(deg)
}
fn exif_gps(bytes: &[u8]) -> Option<(f64, f64)> {
    if let Some(ex) = exif_of(bytes) {
        if let (Some(lat), Some(lng)) = (
            gps_coord(&ex, exif::Tag::GPSLatitude, exif::Tag::GPSLatitudeRef),
            gps_coord(&ex, exif::Tag::GPSLongitude, exif::Tag::GPSLongitudeRef),
        ) {
            return Some((lat, lng));
        }
    }
    fb_exif_gps(bytes)
}
fn exif_date(bytes: &[u8]) -> Option<String> {
    if let Some(ex) = exif_of(bytes) {
        if let Some(f) = ex
            .get_field(exif::Tag::DateTimeOriginal, exif::In::PRIMARY)
            .or_else(|| ex.get_field(exif::Tag::DateTime, exif::In::PRIMARY))
        {
            if let exif::Value::Ascii(a) = &f.value {
                if let Some(s) = a.first() {
                    let s = String::from_utf8_lossy(s);
                    // "2026:06:19 14:30:00" -> "2026.06"（只到月，具体日不落库，避免泄露隐私）
                    let full = s.split(' ').next().unwrap_or("").replace(':', ".");
                    let ym: String = full.split('.').take(2).collect::<Vec<_>>().join(".");
                    if ym.len() >= 6 {
                        return Some(ym);
                    }
                }
            }
        }
    }
    fb_exif_date(bytes)
}
fn exif_orientation(bytes: &[u8]) -> u16 {
    if let Some(ex) = exif_of(bytes) {
        if let Some(f) = ex.get_field(exif::Tag::Orientation, exif::In::PRIMARY) {
            if let Some(v) = f.value.get_uint(0) {
                return v as u16;
            }
        }
    }
    fb_exif_orientation(bytes).unwrap_or(1)
}

// ---------- 宽容 EXIF fallback ----------
// piexif/Pillow 一族工具写出的 EXIF 省略子 IFD 末尾的 next-IFD 指针（TIFF 规范灰区，
// 多数解析器容忍），kamadak-exif 会整体报 InvalidFormat("Unexpected next IFD") 拒绝，
// 导致 GPS/日期/方向全读不出、照片批量进未归位。这里手写最小 TIFF 读取兜底：
// 只按需查 tag、不追 next-IFD 链、任何越界/异常直接放弃返回 None。
struct TiffLite<'a> {
    d: &'a [u8],
    be: bool,
}
impl<'a> TiffLite<'a> {
    fn u16(&self, o: usize) -> Option<u16> {
        let b = self.d.get(o..o + 2)?;
        Some(if self.be { u16::from_be_bytes([b[0], b[1]]) } else { u16::from_le_bytes([b[0], b[1]]) })
    }
    fn u32(&self, o: usize) -> Option<u32> {
        let b = self.d.get(o..o + 4)?;
        let a = [b[0], b[1], b[2], b[3]];
        Some(if self.be { u32::from_be_bytes(a) } else { u32::from_le_bytes(a) })
    }
    // 在 ifd（字节偏移）里线性找 tag，返回 (type, count, 值区偏移)；值 ≤4 字节时内联在条目里
    fn find(&self, ifd: usize, tag: u16) -> Option<(u16, u32, usize)> {
        let n = self.u16(ifd)? as usize;
        if n > 512 {
            return None;
        }
        for i in 0..n {
            let e = ifd + 2 + i * 12;
            if self.u16(e)? == tag {
                let typ = self.u16(e + 2)?;
                let cnt = self.u32(e + 4)?;
                let unit: u32 = match typ {
                    1 | 2 | 7 => 1,
                    3 => 2,
                    4 | 9 => 4,
                    5 | 10 => 8,
                    _ => return None,
                };
                let total = unit.checked_mul(cnt)?;
                let off = if total <= 4 { e + 8 } else { self.u32(e + 8)? as usize };
                return Some((typ, cnt, off));
            }
        }
        None
    }
    fn sub_ifd(&self, ifd0: usize, tag: u16) -> Option<usize> {
        let (_, _, off) = self.find(ifd0, tag)?;
        Some(self.u32(off)? as usize)
    }
    fn ascii(&self, ifd: usize, tag: u16) -> Option<String> {
        let (typ, cnt, off) = self.find(ifd, tag)?;
        if typ != 2 {
            return None;
        }
        let b = self.d.get(off..off + cnt as usize)?;
        Some(String::from_utf8_lossy(b).trim_end_matches('\0').to_string())
    }
    fn rat(&self, off: usize) -> Option<f64> {
        let num = self.u32(off)? as f64;
        let den = self.u32(off + 4)? as f64;
        if den == 0.0 {
            return None;
        }
        Some(num / den)
    }
    // 度分秒三连 Rational → 十进制度
    fn dms(&self, ifd: usize, tag: u16) -> Option<f64> {
        let (typ, cnt, off) = self.find(ifd, tag)?;
        if typ != 5 || cnt < 3 {
            return None;
        }
        Some(self.rat(off)? + self.rat(off + 8)? / 60.0 + self.rat(off + 16)? / 3600.0)
    }
    fn short(&self, ifd: usize, tag: u16) -> Option<u16> {
        let (typ, _, off) = self.find(ifd, tag)?;
        if typ != 3 {
            return None;
        }
        self.u16(off)
    }
}
// 从 JPEG 提取 APP1 "Exif\0\0" 的 TIFF 段（裸 TIFF 直接用），返回 (解析器, IFD0 偏移)
fn tiff_lite(bytes: &[u8]) -> Option<(TiffLite<'_>, usize)> {
    let tiff: &[u8] = if bytes.len() >= 8 && (&bytes[..2] == b"II" || &bytes[..2] == b"MM") {
        bytes
    } else {
        if bytes.get(..2)? != [0xFF, 0xD8] {
            return None;
        }
        let mut p = 2usize;
        let mut found: Option<&[u8]> = None;
        while p + 4 <= bytes.len() {
            if bytes[p] != 0xFF {
                break;
            }
            let marker = bytes[p + 1];
            if marker == 0xDA || marker == 0xD9 {
                break; // SOS/EOI，后面不会再有 APP1
            }
            let len = u16::from_be_bytes([bytes[p + 2], bytes[p + 3]]) as usize;
            if len < 2 || p + 2 + len > bytes.len() {
                break;
            }
            if marker == 0xE1 && bytes.get(p + 4..p + 10) == Some(b"Exif\0\0".as_slice()) {
                found = Some(&bytes[p + 10..p + 2 + len]);
                break;
            }
            p += 2 + len;
        }
        found?
    };
    if tiff.len() < 8 || (&tiff[..2] != b"II" && &tiff[..2] != b"MM") {
        return None;
    }
    let t = TiffLite { d: tiff, be: &tiff[..2] == b"MM" };
    let ifd0 = t.u32(4)? as usize;
    Some((t, ifd0))
}
fn fb_exif_gps(bytes: &[u8]) -> Option<(f64, f64)> {
    let (t, ifd0) = tiff_lite(bytes)?;
    let gps = t.sub_ifd(ifd0, 0x8825)?; // GPS IFD pointer
    let mut lat = t.dms(gps, 0x0002)?;
    let mut lng = t.dms(gps, 0x0004)?;
    if t.ascii(gps, 0x0001).map_or(false, |r| r.starts_with('S')) {
        lat = -lat;
    }
    if t.ascii(gps, 0x0003).map_or(false, |r| r.starts_with('W')) {
        lng = -lng;
    }
    if !(lat.is_finite() && lng.is_finite() && lat.abs() <= 90.0 && lng.abs() <= 180.0) {
        return None;
    }
    Some((lat, lng))
}
fn fb_exif_date(bytes: &[u8]) -> Option<String> {
    let (t, ifd0) = tiff_lite(bytes)?;
    // Exif IFD 的 DateTimeOriginal，退而求 IFD0 的 DateTime
    let s = t
        .sub_ifd(ifd0, 0x8769)
        .and_then(|e| t.ascii(e, 0x9003))
        .or_else(|| t.ascii(ifd0, 0x0132))?;
    let full = s.split(' ').next().unwrap_or("").replace(':', ".");
    let ym: String = full.split('.').take(2).collect::<Vec<_>>().join(".");
    if ym.len() >= 6 { Some(ym) } else { None }
}
fn fb_exif_orientation(bytes: &[u8]) -> Option<u16> {
    let (t, ifd0) = tiff_lite(bytes)?;
    t.short(ifd0, 0x0112)
}

// HEIC/HEIF 探测：ISO-BMFF ftyp box，major brand 常见 heic/heix/mif1/msf1/hevc 等。
// 用于上传失败时给 iPhone 用户精准提示（image crate 不解 HEIC）。
fn looks_like_heic(bytes: &[u8]) -> bool {
    if bytes.len() < 12 || &bytes[4..8] != b"ftyp" {
        return false;
    }
    matches!(
        &bytes[8..12],
        b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx"
            | b"mif1" | b"msf1" | b"avif" | b"avis"
    )
}
// HEIC/HEIF/AVIF 解码（iPhone 原图）：外部命令 heif-convert（libheif 1.17）转 JPG 再交给 image crate。
// 用外部命令而非 FFI，规避 libheif-rs 需要 libheif≥1.18、系统仅 1.17 的版本鸿沟。
// heif-convert 对单图写精确文件名、多图（Live/连拍）写 {stem}-1.jpg…，取主图=优先精确名、否则 -1。
fn decode_heic(bytes: &[u8]) -> anyhow::Result<image::DynamicImage> {
    let id = uid();
    let dir = cache_dir();
    let _ = fs::create_dir_all(&dir);
    let tin = dir.join(format!("heic_{id}.heic"));
    let tout = dir.join(format!("heic_{id}.jpg"));
    fs::write(&tin, bytes)?;
    let run = std::process::Command::new("heif-convert").arg(&tin).arg(&tout).output();
    let _ = fs::remove_file(&tin);
    let out = run.map_err(|e| anyhow::anyhow!("heif-convert 无法执行：{e}"))?;
    // 主图候选：精确名 → -1 后缀
    let alt = dir.join(format!("heic_{id}-1.jpg"));
    let produced = if tout.exists() {
        tout.clone()
    } else if alt.exists() {
        alt.clone()
    } else {
        anyhow::bail!(
            "heif-convert 未产出图片（{}）",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    };
    let data = fs::read(&produced)?;
    // 清理所有产物（含多图 -2/-3…）
    for i in 0..8 {
        let f = if i == 0 { tout.clone() } else { dir.join(format!("heic_{id}-{i}.jpg")) };
        let _ = fs::remove_file(&f);
    }
    Ok(image::load_from_memory(&data)?)
}
// 解码 + 按 EXIF orient 摆正（HEIC 已由 heif-convert 摆正，不重复转）
fn decode_oriented(bytes: &[u8], orient: u16) -> anyhow::Result<image::DynamicImage> {
    let is_heic = looks_like_heic(bytes);
    let mut img = if is_heic {
        decode_heic(bytes)?
    } else {
        image::load_from_memory(bytes)?
    };
    if !is_heic {
        img = match orient {
            3 => img.rotate180(),
            6 => img.rotate90(),
            8 => img.rotate270(),
            _ => img,
        };
    }
    Ok(img)
}
fn encode_jpeg(img: &image::DynamicImage, max: u32, quality: u8) -> anyhow::Result<Vec<u8>> {
    let resized = img.resize(max, max, image::imageops::FilterType::Lanczos3);
    let rgb = resized.to_rgb8();
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality)
        .encode(rgb.as_raw(), rgb.width(), rgb.height(), ExtendedColorType::Rgb8)?;
    Ok(out)
}
// 情绪色卡：从封面照片提取一个有代表性的主色调（hex）。
// 缩到 40px 求平均，但跳过接近纯黑/纯白的像素（天空、暗角、逆光会把均值拉灰），
// 保留有色彩的中间调；再轻微提亮避免偏暗。全被跳过（黑白图）则回落暖灰。
fn dominant_color_of(img: &image::DynamicImage) -> String {
    let small = img.resize(40, 40, image::imageops::FilterType::Triangle).to_rgb8();
    let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
    for px in small.pixels() {
        let (pr, pg, pb) = (px[0] as u32, px[1] as u32, px[2] as u32);
        let sum = pr + pg + pb;
        if sum < 90 || sum > 690 {
            continue; // 太暗/太亮，无色彩信息
        }
        r += pr as u64;
        g += pg as u64;
        b += pb as u64;
        n += 1;
    }
    if n == 0 {
        // 全是极端明暗（纯黑白图）→ 用全图平均兜底
        for px in small.pixels() {
            r += px[0] as u64;
            g += px[1] as u64;
            b += px[2] as u64;
            n += 1;
        }
        if n == 0 {
            return "#8a8172".into();
        }
    }
    let boost = |v: u64| -> u8 {
        let x = (v / n) as f64 * 1.08; // 轻微提亮
        x.min(235.0) as u8 // 压一点上限，避免死白
    };
    format!("#{:02x}{:02x}{:02x}", boost(r), boost(g), boost(b))
}
fn process_image(bytes: &[u8], max: u32, quality: u8, orient: u16) -> anyhow::Result<Vec<u8>> {
    encode_jpeg(&decode_oriented(bytes, orient)?, max, quality)
}

// ---------- 反查地名 ----------
// 归程分组名：优先用行政大区（都/府/县/州）并去掉级别后缀，让"港区/新宿区"都归到"东京"，
// 避免大都市被 Nominatim 的 ward 级 city 字段拆成一堆区级旅程。region 空则回落 city。
// em 字段消毒：去控制字符和尖括号（纵深防御，配合前端 esc），限 8 字防撑破 marker 布局。
fn sanitize_em(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() && *c != '<' && *c != '>')
        .take(8)
        .collect()
}
fn trip_group_name(region: &str, city: &str) -> String {
    let base = if !region.trim().is_empty() { region.trim() } else { city.trim() };
    if base.chars().count() > 2 {
        for suf in ["都", "府", "県", "省", "市", "区", "區", "縣", "县", "町", "村"] {
            if let Some(x) = base.strip_suffix(suf) {
                if !x.is_empty() {
                    return x.to_string();
                }
            }
        }
    }
    base.to_string()
}
async fn reverse_geocode(
    http: &reqwest::Client,
    lat: f64,
    lng: f64,
) -> Option<(String, String, String, String)> {
    let url = format!(
        "https://nominatim.openstreetmap.org/reverse?lat={}&lon={}&format=json&accept-language=zh-CN,zh",
        lat, lng
    );
    let resp = http.get(&url).send().await.ok()?;
    let j: Value = resp.json().await.ok()?;
    let a = j.get("address")?;
    let pick = |keys: &[&str]| -> String {
        for k in keys {
            if let Some(v) = a.get(*k).and_then(|x| x.as_str()) {
                return v.to_string();
            }
        }
        String::new()
    };
    let raw_city = pick(&["city", "town", "county", "state", "country"]);
    // 行政大区：优先 state/province 文本；日本的地址常缺 state 字段（东京 23 区把 city 占成"××區"），
    // 但稳定带 ISO3166-2-lvl4（如 JP-13），用都道府县码表兜底还原"东京"级城市名
    let mut region = pick(&["state", "province", "region"]);
    if region.is_empty() {
        if let Some(iso) = a.get("ISO3166-2-lvl4").and_then(|x| x.as_str()) {
            if let Some(pref) = jp_pref_name(iso) {
                region = pref.to_string();
            }
        }
    }
    // 城市归一：区级（港區/澀谷區）升到城市级（东京），"××市"去后缀 → 城市统计不再按区碎裂
    let city = normalize_city(&raw_city, &region);
    let name = pick(&[
        "tourism", "attraction", "leisure", "building", "road", "suburb", "neighbourhood",
    ]);
    let country = pick(&["country"]);
    let name = if name.is_empty() { city.clone() } else { name };
    // Nominatim 名称可能带「A;B」多语言/别名变体，取第一段
    let name = name.split(';').next().unwrap_or("").trim().to_string();
    Some((
        city,
        if name.is_empty() { "新地点".into() } else { name },
        country,
        region,
    ))
}

// 日本 47 都道府县 ISO3166-2 码 → 中文名（东京 23 区没有 state 字段时的兜底）
fn jp_pref_name(iso: &str) -> Option<&'static str> {
    Some(match iso {
        "JP-01" => "北海道", "JP-02" => "青森", "JP-03" => "岩手", "JP-04" => "宫城",
        "JP-05" => "秋田", "JP-06" => "山形", "JP-07" => "福岛", "JP-08" => "茨城",
        "JP-09" => "栃木", "JP-10" => "群马", "JP-11" => "埼玉", "JP-12" => "千叶",
        "JP-13" => "东京", "JP-14" => "神奈川", "JP-15" => "新潟", "JP-16" => "富山",
        "JP-17" => "石川", "JP-18" => "福井", "JP-19" => "山梨", "JP-20" => "长野",
        "JP-21" => "岐阜", "JP-22" => "静冈", "JP-23" => "爱知", "JP-24" => "三重",
        "JP-25" => "滋贺", "JP-26" => "京都", "JP-27" => "大阪", "JP-28" => "兵库",
        "JP-29" => "奈良", "JP-30" => "和歌山", "JP-31" => "鸟取", "JP-32" => "岛根",
        "JP-33" => "冈山", "JP-34" => "广岛", "JP-35" => "山口", "JP-36" => "德岛",
        "JP-37" => "香川", "JP-38" => "爱媛", "JP-39" => "高知", "JP-40" => "福冈",
        "JP-41" => "佐贺", "JP-42" => "长崎", "JP-43" => "熊本", "JP-44" => "大分",
        "JP-45" => "宫崎", "JP-46" => "鹿儿岛", "JP-47" => "冲绳",
        _ => return None,
    })
}

// 城市归一：区级名（以 区/區 结尾）且已知所属大区 → 用大区名；再去 都/府/県/省/市 等行政后缀。
// "港區"+region"东京" → "东京"；"大阪市" → "大阪"；"杭州市" → "杭州"；短名（≤2 字）不动避免"京都"被削成"京"
fn normalize_city(city: &str, region: &str) -> String {
    let c = city.trim();
    let base = if (c.ends_with('区') || c.ends_with('區')) && !region.trim().is_empty() {
        region.trim()
    } else {
        c
    };
    if base.chars().count() > 2 {
        for suf in ["都", "府", "県", "省", "市", "縣", "县"] {
            if let Some(x) = base.strip_suffix(suf) {
                if !x.is_empty() {
                    return x.to_string();
                }
            }
        }
    }
    base.to_string()
}

// ---------- 鉴权 ----------
// 定长时间字符串比较，避免按前缀命中的计时侧信道
fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}
async fn login(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    const MAX_FAILS: u32 = 8;
    const LOCK_SECS: u64 = 900; // 连续失败达阈值锁 15 分钟
    let ip = client_ip(&headers);
    let now = now_secs();
    // 锁定检查（先取判断结果并释放锁，再 await——std MutexGuard 不能跨 await 持有）
    let locked = {
        let guard = st.login_guard.lock().unwrap();
        guard
            .get(&ip)
            .map(|(fails, until)| *fails >= MAX_FAILS && *until > now)
            .unwrap_or(false)
    };
    if locked {
        tokio::time::sleep(Duration::from_millis(500)).await;
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"ok": false, "error": "尝试过于频繁，请稍后再试"})),
        )
            .into_response();
    }
    let admin_pass = std::env::var("ADMIN_PASSWORD").unwrap_or_default();
    let given = body.get("password").and_then(|v| v.as_str()).unwrap_or("");
    if admin_pass.is_empty() || given.is_empty() || !ct_eq(given, &admin_pass) {
        {
            let mut guard = st.login_guard.lock().unwrap();
            let e = guard.entry(ip).or_insert((0, 0));
            // 锁定过期后重新计数
            if e.1 <= now {
                e.0 = 0;
            }
            e.0 += 1;
            e.1 = now + LOCK_SECS;
            if guard.len() > 10_000 {
                guard.retain(|_, v| v.1 > now); // 防表无限增长
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        return (StatusCode::UNAUTHORIZED, Json(json!({"ok": false}))).into_response();
    }
    // 成功：清除该 IP 失败计数
    st.login_guard.lock().unwrap().remove(&ip);
    let token = new_token();
    let now = now_secs();
    {
        let mut ss = st.sessions.lock().unwrap();
        ss.retain(|e| e.exp > now);
        ss.push(SessionEntry { hash: sha256_hex(token.as_bytes()), exp: now + SESSION_MAX_AGE });
        save_sessions(&ss);
    }
    let cookie = format!(
        "session={token}; HttpOnly; Secure; SameSite=Lax; Max-Age={SESSION_MAX_AGE}; Path=/"
    );
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({"ok": true})),
    )
        .into_response()
}

async fn logout(State(st): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if let Some(tok) = cookie_token(&headers) {
        let h = sha256_hex(tok.as_bytes());
        let mut ss = st.sessions.lock().unwrap();
        ss.retain(|e| e.hash != h);
        save_sessions(&ss);
    }
    let cookie = "session=; HttpOnly; Secure; SameSite=Lax; Max-Age=0; Path=/".to_string();
    (
        StatusCode::OK,
        [(header::SET_COOKIE, cookie)],
        Json(json!({"ok": true})),
    )
}

async fn me(State(st): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    Json(json!({"owner": is_owner(&st, &headers)}))
}

// ---------- 路由处理 ----------
async fn health() -> impl IntoResponse {
    Json(json!({"ok": true, "style": ofm_style()}))
}

async fn index_html() -> impl IntoResponse {
    match fs::read("public/index.html") {
        Ok(b) => (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "text/html; charset=utf-8"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            b,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "index.html not found").into_response(),
    }
}

// 访客视角脱敏：坐标量化到 ~2 位小数（约 1km，够看足迹分布但不暴露到楼栋），
// 日期截断到年月（隐私，在 API 边界执行，不靠客户端 dispDate 遮蔽）。
fn sanitize_place_for_guest(p: &Place) -> Place {
    let mut q = p.clone();
    q.lat = (q.lat * 100.0).round() / 100.0;
    q.lng = (q.lng * 100.0).round() / 100.0;
    q.date = q.date.as_deref().map(trunc_ym);
    for v in q.visits.iter_mut() {
        v.date = trunc_ym(&v.date);
    }
    for ph in q.photos.iter_mut() {
        ph.date = ph.date.as_deref().map(trunc_ym);
    }
    q
}
// 'YYYY.MM.DD' / 'YYYY-MM-DD' → 'YYYY.MM'；只有年则保留年
fn trunc_ym(s: &str) -> String {
    let parts: Vec<&str> = s.split(|c| c == '.' || c == '-' || c == '/').collect();
    match parts.as_slice() {
        [y, m, ..] if y.len() == 4 && !m.is_empty() => format!("{y}.{m}"),
        [y, ..] if y.len() == 4 => y.to_string(),
        _ => s.to_string(),
    }
}
// 站点配置（前端「关于」页用）：ICP 备案号来自环境变量 SITE_ICP，留空则前端不显示备案行
fn site_config() -> Value {
    json!({
        "icp": std::env::var("SITE_ICP").unwrap_or_default(),
        "icpUrl": "https://beian.miit.gov.cn/",
    })
}
async fn places(State(st): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let owner = is_owner(&st, &headers);
    let s = st.store.lock().await;
    // 互动数据：点赞对所有人可见（社交证明），访问量仅站主可见
    let engagement = json!({
        "likes": s.likes,
        "msgCount": s.messages.len(),
        "views": if owner { json!(s.views) } else { Value::Null },
    });
    if !s.places.is_empty() {
        let places: Vec<Place> = if owner {
            s.places.clone()
        } else {
            s.places.iter().map(sanitize_place_for_guest).collect()
        };
        return Json(json!({
            "places": places, "cities": s.cities, "trips": s.trips, "profile": s.profile,
            "engagement": engagement, "site": site_config()
        }));
    }
    // 空库回退到内置样例仅用于演示，且必须显式开启 DEMO_SEED=1。
    // 否则空库如实返回空数据（保留真实 trips/cities/profile）——避免删到最后一个地点时样例“复活”、真实旅程被样例数据覆盖。
    if std::env::var("DEMO_SEED").as_deref() == Ok("1") {
        let profile = s.profile.clone();
        drop(s);
        let seed: Store = serde_json::from_str(SEED).unwrap();
        let places: Vec<Place> = if owner {
            seed.places
        } else {
            seed.places.iter().map(sanitize_place_for_guest).collect()
        };
        return Json(json!({
            "places": places, "cities": seed.cities, "trips": seed.trips, "profile": profile,
            "engagement": engagement, "site": site_config()
        }));
    }
    Json(json!({
        "places": [], "cities": s.cities, "trips": s.trips, "profile": s.profile,
        "engagement": engagement, "site": site_config()
    }))
}

// PATCH /api/profile — 站主名片（name ≤30 字、bio ≤80 字，超长截断）
async fn patch_profile(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    if !is_owner(&st, &headers) {
        return unauthorized();
    }
    let cap = |s: &str, n: usize| s.trim().chars().take(n).collect::<String>();
    let mut store = st.store.lock().await;
    if let Some(v) = body.get("name").and_then(|v| v.as_str()) {
        store.profile.name = cap(v, 30);
    }
    if let Some(v) = body.get("bio").and_then(|v| v.as_str()) {
        store.profile.bio = cap(v, 80);
    }
    if let Some(v) = body.get("about").and_then(|v| v.as_str()) {
        store.profile.about = cap(v, 2000); // 关于页长文，2000 字上限
    }
    let resp = store.profile.clone();
    save_store(&store);
    Json(resp).into_response()
}

// 文本消毒：去控制字符（保留换行）、限长；配合前端 esc() 防 XSS
fn sanitize_text(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .take(max)
        .collect::<String>()
        .trim()
        .to_string()
}

// POST /api/places/:id/like —— 访客点赞（同 IP 对同点只 +1，配合前端 localStorage）
async fn like_place(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> axum::response::Response {
    let ip = client_ip(&headers);
    let key = format!("{ip}|{id}");
    let already = {
        let mut g = st.like_guard.lock().unwrap();
        if g.contains(&key) {
            true
        } else {
            g.insert(key);
            if g.len() > 200_000 {
                g.clear();
            }
            false
        }
    };
    let mut store = st.store.lock().await;
    if !store.places.iter().any(|p| p.id == id) {
        return (StatusCode::NOT_FOUND, Json(json!({"ok": false, "error": "地点不存在"})))
            .into_response();
    }
    if already {
        let n = store.likes.get(&id).copied().unwrap_or(0);
        return Json(json!({"ok": true, "likes": n, "already": true})).into_response();
    }
    let n = store.likes.entry(id.clone()).or_insert(0);
    *n += 1;
    let cnt = *n;
    save_store(&store);
    Json(json!({"ok": true, "likes": cnt})).into_response()
}

// GET /api/messages —— 公开留言列表（时间倒序，上限 500）
async fn list_messages(State(st): State<AppState>) -> impl IntoResponse {
    let s = st.store.lock().await;
    let mut msgs: Vec<GuestMessage> = s.messages.clone();
    msgs.sort_by(|a, b| b.ts.cmp(&a.ts));
    msgs.truncate(500);
    Json(json!({"messages": msgs}))
}

// POST /api/messages —— 访客留言（每 IP 20 秒一条，name≤20 / text≤500）
async fn post_message(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let ip = client_ip(&headers);
    let now = now_secs();
    let too_soon = {
        let mut g = st.msg_guard.lock().unwrap();
        let soon = g.get(&ip).map(|&last| now < last + 20).unwrap_or(false);
        if !soon {
            g.insert(ip.clone(), now);
            if g.len() > 50_000 {
                g.retain(|_, &mut t| t + 3600 > now);
            }
        }
        soon
    };
    if too_soon {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({"ok": false, "error": "留言太频繁，请过一会儿再来"})),
        )
            .into_response();
    }
    let name = {
        let n = sanitize_text(body.get("name").and_then(|v| v.as_str()).unwrap_or(""), 20);
        if n.is_empty() {
            "访客".to_string()
        } else {
            n
        }
    };
    let text = sanitize_text(body.get("text").and_then(|v| v.as_str()).unwrap_or(""), 500);
    if text.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"ok": false, "error": "留言内容为空"})))
            .into_response();
    }
    let place_id = body
        .get("placeId")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let msg = GuestMessage { id: format!("m_{}", uid()), name, text, ts: now, place_id };
    let mut store = st.store.lock().await;
    store.messages.push(msg.clone());
    let over = store.messages.len().saturating_sub(5000);
    if over > 0 {
        store.messages.drain(0..over); // 最多留 5000 条
    }
    save_store(&store);
    Json(json!({"ok": true, "message": msg})).into_response()
}

// DELETE /api/messages/:id —— 站主删留言
async fn delete_message(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> axum::response::Response {
    if !is_owner(&st, &headers) {
        return unauthorized();
    }
    let mut store = st.store.lock().await;
    let before = store.messages.len();
    store.messages.retain(|m| m.id != id);
    if store.messages.len() != before {
        save_store(&store);
    }
    Json(json!({"ok": true})).into_response()
}

// POST /api/view —— 访客访问计数（不计站主；每 5 次落盘一次减少写放大）
async fn track_view(State(st): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    if is_owner(&st, &headers) {
        return Json(json!({"ok": true, "skipped": true}));
    }
    let mut store = st.store.lock().await;
    store.views += 1;
    if store.views % 5 == 0 {
        save_store(&store);
    }
    Json(json!({"ok": true}))
}

// 分享页 OG 注入：/p/:id 单地点、/t/:id 整趟旅程。爬虫拿到带 og 的 HTML，人肉打开由脚本跳到 SPA hash。
fn attr_esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}
// 分享路径里的 id 只允许 [A-Za-z0-9_-]（我们生成的 id 本就是这个字符集）。
// 任何越界字符一律拒绝，从源头掐断把 id 拼进内联 <script> 的注入面。
fn safe_share_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}
// photos/ 下的相对路径是否安全：非空、不以 / 开头（绝对路径）、不含 .. 段、不含反斜杠或盘符
fn is_safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && !rel.contains('\\')
        && !rel.split('/').any(|seg| seg == ".." || seg == "." || seg.is_empty())
}
async fn share_place_page(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    if !safe_share_id(&id) {
        return render_share_page("旅行足迹", "", None, "#").await;
    }
    let (title, desc, image) = {
        let s = st.store.lock().await;
        match s.places.iter().find(|p| p.id == id) {
            Some(p) => {
                let img = p
                    .cover
                    .clone()
                    .or_else(|| p.photos.first().map(|ph| ph.url.clone()));
                (p.name.clone(), if p.feel.is_empty() { p.city.clone() } else { p.feel.clone() }, img)
            }
            None => ("旅行足迹".into(), String::new(), None),
        }
    };
    render_share_page(&title, &desc, image.as_deref(), &format!("#p/{id}")).await
}
async fn share_trip_page(
    State(st): State<AppState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    if !safe_share_id(&id) {
        return render_share_page("旅行足迹", "", None, "#").await;
    }
    let (title, desc, image) = {
        let s = st.store.lock().await;
        match s.trips.iter().find(|t| t.id == id) {
            Some(t) => {
                let members: Vec<&Place> = s.places.iter().filter(|p| p.trip_id.as_deref() == Some(&t.id)).collect();
                let img = members
                    .iter()
                    .find_map(|p| p.cover.clone().or_else(|| p.photos.first().map(|ph| ph.url.clone())));
                (format!("{} · 旅程", t.name), format!("这趟旅程记录了 {} 个地点", members.len()), img)
            }
            None => ("旅行足迹".into(), String::new(), None),
        }
    };
    render_share_page(&title, &desc, image.as_deref(), &format!("#t/{id}")).await
}
async fn render_share_page(
    title: &str,
    desc: &str,
    image: Option<&str>,
    hash: &str,
) -> axum::response::Response {
    let html = match fs::read_to_string("public/index.html") {
        Ok(h) => h,
        Err(_) => return (StatusCode::NOT_FOUND, "index.html not found").into_response(),
    };
    let origin = std::env::var("SITE_ORIGIN").unwrap_or_else(|_| "https://atlas.sol42.cn".into());
    let img_abs = image
        .map(|u| if u.starts_with("http") { u.to_string() } else { format!("{origin}{u}") })
        .unwrap_or_else(|| format!("{origin}/icons/icon-512.png"));
    let og = format!(
        "<meta property=\"og:type\" content=\"website\">\
         <meta property=\"og:title\" content=\"{t}\">\
         <meta property=\"og:description\" content=\"{d}\">\
         <meta property=\"og:image\" content=\"{img}\">\
         <meta name=\"twitter:card\" content=\"summary_large_image\">\
         <script>try{{if(!location.hash)location.replace(location.pathname.replace(/\\/(p|t)\\/.*$/,'/')+{h});}}catch(e){{}}</script>",
        t = attr_esc(title),
        d = attr_esc(desc),
        img = attr_esc(&img_abs),
        // hash 以 JSON 字符串字面量注入（自带引号并转义 '"\ 等），JS 上下文安全
        h = serde_json::to_string(hash).unwrap_or_else(|_| "\"#\"".into())
    );
    let html = html.replacen("</head>", &format!("{og}</head>"), 1);
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        html,
    )
        .into_response()
}

// POST /api/places
async fn create_place(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    if !is_owner(&st, &headers) {
        return unauthorized();
    }
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    let lat = body.get("lat").and_then(|v| v.as_f64());
    let lng = body.get("lng").and_then(|v| v.as_f64());
    let (Some(lat), Some(lng)) = (lat, lng) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "lat/lng required"}))).into_response();
    };
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "name required"}))).into_response();
    }
    let status = match body.get("status").and_then(|v| v.as_str()) {
        Some("visited") => "visited".to_string(),
        _ => "planned".to_string(),
    };
    // category 白名单校验：非法值报 400
    let category = match body.get("category").and_then(|v| v.as_str()) {
        None => String::new(),
        Some(c) if is_valid_category(c) => c.to_string(),
        Some(_) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid category"}))).into_response();
        }
    };
    let place = Place {
        id: format!("p_{}", uid()),
        lat,
        lng,
        short: body
            .get("short")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&name)
            .trim()
            .to_string(),
        name,
        city: body.get("city").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        country: body.get("country").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        em: sanitize_em(body.get("em").and_then(|v| v.as_str()).unwrap_or("📍")),
        date: body.get("date").and_then(|v| v.as_str()).map(String::from),
        rating: body.get("rating").and_then(|v| v.as_u64()).unwrap_or(0).min(5) as u8,
        tags: body
            .get("tags")
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|t| t.as_str().map(String::from)).collect())
            .unwrap_or_default(),
        feel: body.get("feel").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        photos: vec![],
        cover: body.get("cover").and_then(|v| v.as_str()).map(String::from),
        status,
        trip_id: body.get("tripId").and_then(|v| v.as_str()).map(String::from),
        visits: vec![],
        category,
        color: None,
        guide: Value::Null,
    };
    let mut store = st.store.lock().await;
    store.places.push(place.clone());
    save_store(&store);
    Json(place).into_response()
}

// PATCH /api/places/:id
async fn patch_place(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    if !is_owner(&st, &headers) {
        return unauthorized();
    }
    // 先校验 visits 结构（若提供），避免部分字段已改而校验失败导致内存/磁盘不一致
    let parsed_visits: Option<Vec<Visit>> = match body.get("visits") {
        None => None,
        Some(Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for (i, item) in arr.iter().enumerate() {
                let Some(o) = item.as_object() else {
                    return (StatusCode::BAD_REQUEST, Json(json!({"error": "visits element must be object"}))).into_response();
                };
                let date = match o.get("date") {
                    None | Some(Value::Null) => String::new(),
                    Some(Value::String(s)) => s.clone(),
                    _ => return (StatusCode::BAD_REQUEST, Json(json!({"error": "visit date must be string"}))).into_response(),
                };
                let note = match o.get("note") {
                    None | Some(Value::Null) => String::new(),
                    Some(Value::String(s)) => s.clone(),
                    _ => return (StatusCode::BAD_REQUEST, Json(json!({"error": "visit note must be string"}))).into_response(),
                };
                let rating = o.get("rating").and_then(|r| r.as_u64()).unwrap_or(0).min(5) as u8;
                let vid = o
                    .get("id")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| format!("v_{}_{}", uid(), i));
                out.push(Visit { id: vid, date, note, rating });
            }
            Some(out)
        }
        Some(_) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": "visits must be array"}))).into_response();
        }
    };
    // 先校验 category（若提供），避免部分字段已改而校验失败导致不一致
    let parsed_category: Option<String> = match body.get("category") {
        None => None,
        Some(Value::String(c)) if is_valid_category(c) => Some(c.clone()),
        Some(Value::Null) => Some(String::new()),
        Some(_) => {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid category"}))).into_response();
        }
    };
    let mut store = st.store.lock().await;
    let Some(p) = store.places.iter_mut().find(|p| p.id == id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "place not found"}))).into_response();
    };
    if let Some(v) = body.get("name").and_then(|v| v.as_str()) {
        if !v.trim().is_empty() {
            p.name = v.trim().to_string();
        }
    }
    if let Some(v) = body.get("short").and_then(|v| v.as_str()) {
        p.short = v.trim().to_string();
    }
    if let Some(v) = body.get("em").and_then(|v| v.as_str()) {
        p.em = sanitize_em(v);
    }
    if let Some(v) = body.get("feel").and_then(|v| v.as_str()) {
        p.feel = v.to_string();
    }
    if let Some(v) = body.get("city").and_then(|v| v.as_str()) {
        p.city = v.to_string();
    }
    if let Some(v) = body.get("country").and_then(|v| v.as_str()) {
        p.country = v.to_string();
    }
    if let Some(v) = body.get("rating").and_then(|v| v.as_u64()) {
        p.rating = v.min(5) as u8;
    }
    if let Some(v) = body.get("tags").and_then(|v| v.as_array()) {
        p.tags = v.iter().filter_map(|t| t.as_str().map(String::from)).collect();
    }
    if let Some(v) = body.get("lat").and_then(|v| v.as_f64()) {
        p.lat = v;
    }
    if let Some(v) = body.get("lng").and_then(|v| v.as_f64()) {
        p.lng = v;
    }
    if let Some(v) = body.get("date") {
        p.date = if v.is_null() { None } else { v.as_str().map(String::from) };
    }
    if let Some(v) = body.get("cover") {
        p.cover = if v.is_null() { None } else { v.as_str().map(String::from) };
    }
    if let Some(v) = body.get("tripId") {
        p.trip_id = if v.is_null() { None } else { v.as_str().map(String::from) };
    }
    if let Some(v) = body.get("status").and_then(|v| v.as_str()) {
        if v == "visited" || v == "planned" {
            p.status = v.to_string();
        }
    }
    if let Some(cat) = parsed_category {
        p.category = cat;
    }
    if let Some(vs) = parsed_visits {
        p.visits = vs;
        // 后端同步维护：place.date = visits 中最早的非空日期；无非空日期则保留原值
        if let Some(min) = p
            .visits
            .iter()
            .map(|v| v.date.as_str())
            .filter(|d| !d.is_empty())
            .min()
        {
            p.date = Some(min.to_string());
        }
    }
    // 攻略字段：前端定义 shape，后端透明存取（null 清空）
    if let Some(v) = body.get("guide") {
        p.guide = v.clone();
    }
    let resp = p.clone();
    save_store(&store);
    Json(resp).into_response()
}

// DELETE /api/places/:id
async fn delete_place(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> axum::response::Response {
    if !is_owner(&st, &headers) {
        return unauthorized();
    }
    let mut store = st.store.lock().await;
    let Some(pos) = store.places.iter().position(|p| p.id == id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "place not found"}))).into_response();
    };
    let removed = store.places.remove(pos);
    save_store(&store);
    drop(store);
    for ph in &removed.photos {
        delete_photo_files(&ph.id);
    }
    Json(json!({"ok": true})).into_response()
}

// DELETE /api/places/:id/photos/:photoId
async fn delete_place_photo(
    State(st): State<AppState>,
    Path((id, photo_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> axum::response::Response {
    if !is_owner(&st, &headers) {
        return unauthorized();
    }
    let mut store = st.store.lock().await;
    let Some(p) = store.places.iter_mut().find(|p| p.id == id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "place not found"}))).into_response();
    };
    let Some(pos) = p.photos.iter().position(|ph| ph.id == photo_id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "photo not found"}))).into_response();
    };
    let removed = p.photos.remove(pos);
    if let Some(cover) = &p.cover {
        if cover == &removed.url || cover == &removed.thumb {
            p.cover = None;
        }
    }
    let resp = p.clone();
    save_store(&store);
    drop(store);
    delete_photo_files(&removed.id);
    Json(resp).into_response()
}

// ---------- Trips ----------
// checklist 请求体解析（create/patch 共用）：None=未提供；Err=结构非法
fn parse_checklist(body: &Value) -> Result<Option<Vec<ChecklistItem>>, &'static str> {
    match body.get("checklist") {
        None => Ok(None),
        Some(Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for (i, item) in arr.iter().enumerate() {
                let Some(o) = item.as_object() else {
                    return Err("checklist element must be object");
                };
                let text = match o.get("text") {
                    None | Some(Value::Null) => String::new(),
                    Some(Value::String(s)) => s.clone(),
                    _ => return Err("checklist text must be string"),
                };
                let done = o.get("done").and_then(|d| d.as_bool()).unwrap_or(false);
                let cid = o
                    .get("id")
                    .and_then(|x| x.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(String::from)
                    .unwrap_or_else(|| format!("c_{}_{}", uid(), i));
                out.push(ChecklistItem { id: cid, text, done });
            }
            Ok(Some(out))
        }
        Some(_) => Err("checklist must be array"),
    }
}

// POST /api/trips
async fn create_trip(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    if !is_owner(&st, &headers) {
        return unauthorized();
    }
    let name = body.get("name").and_then(|v| v.as_str()).unwrap_or("").trim().to_string();
    if name.is_empty() {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "name required"}))).into_response();
    }
    let status = match body.get("status").and_then(|v| v.as_str()) {
        Some("done") => "done".to_string(),
        _ => "planned".to_string(),
    };
    // 新建时也接收 checklist（此前硬编码空数组，创建表单里填的清单会被静默丢弃）
    let checklist = match parse_checklist(&body) {
        Ok(cl) => cl.unwrap_or_default(),
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    };
    let trip = Trip {
        id: format!("t_{}", uid()),
        name,
        em: sanitize_em(body.get("em").and_then(|v| v.as_str()).unwrap_or("🧳")),
        color: body.get("color").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        start_date: body.get("startDate").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        end_date: body.get("endDate").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        note: body.get("note").and_then(|v| v.as_str()).unwrap_or("").to_string(),
        status,
        checklist,
        guide: Value::Null,
    };
    let mut store = st.store.lock().await;
    store.trips.push(trip.clone());
    save_store(&store);
    Json(trip).into_response()
}

// PATCH /api/trips/:id
async fn patch_trip(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    if !is_owner(&st, &headers) {
        return unauthorized();
    }
    // 先校验 checklist 结构（若提供）
    let parsed_checklist: Option<Vec<ChecklistItem>> = match parse_checklist(&body) {
        Ok(v) => v,
        Err(e) => return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
    };
    let mut store = st.store.lock().await;
    let Some(t) = store.trips.iter_mut().find(|t| t.id == id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "trip not found"}))).into_response();
    };
    if let Some(v) = body.get("name").and_then(|v| v.as_str()) {
        if !v.trim().is_empty() {
            t.name = v.trim().to_string();
        }
    }
    if let Some(v) = body.get("em").and_then(|v| v.as_str()) {
        t.em = sanitize_em(v);
    }
    if let Some(v) = body.get("color").and_then(|v| v.as_str()) {
        t.color = v.to_string();
    }
    if let Some(v) = body.get("startDate").and_then(|v| v.as_str()) {
        t.start_date = v.to_string();
    }
    if let Some(v) = body.get("endDate").and_then(|v| v.as_str()) {
        t.end_date = v.to_string();
    }
    if let Some(v) = body.get("note").and_then(|v| v.as_str()) {
        t.note = v.to_string();
    }
    if let Some(v) = body.get("status").and_then(|v| v.as_str()) {
        if v == "done" || v == "planned" {
            t.status = v.to_string();
        }
    }
    if let Some(cl) = parsed_checklist {
        t.checklist = cl;
    }
    // 攻略字段：前端定义 shape，后端透明存取（null 清空）
    if let Some(v) = body.get("guide") {
        t.guide = v.clone();
    }
    let resp = t.clone();
    save_store(&store);
    Json(resp).into_response()
}

// DELETE /api/trips/:id — 挂在该 trip 下的 places 的 tripId 置 null，不删 places
async fn delete_trip(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> axum::response::Response {
    if !is_owner(&st, &headers) {
        return unauthorized();
    }
    let mut store = st.store.lock().await;
    let Some(pos) = store.trips.iter().position(|t| t.id == id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "trip not found"}))).into_response();
    };
    store.trips.remove(pos);
    for p in store.places.iter_mut() {
        if p.trip_id.as_deref() == Some(id.as_str()) {
            p.trip_id = None;
        }
    }
    save_store(&store);
    Json(json!({"ok": true})).into_response()
}

// ---------- Unplaced（无 GPS 照片暂存区） ----------
// GET /api/unplaced
async fn list_unplaced(State(st): State<AppState>, headers: HeaderMap) -> axum::response::Response {
    if !is_owner(&st, &headers) {
        return unauthorized();
    }
    let s = st.store.lock().await;
    Json(json!({"unplaced": s.unplaced})).into_response()
}

// POST /api/unplaced/:id/assign — body 二选一：{placeId} 挂现有点；{lat,lng,name,em?,date?,status?} 新建点
async fn assign_unplaced(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    if !is_owner(&st, &headers) {
        return unauthorized();
    }
    let mut store = st.store.lock().await;
    let Some(pos) = store.unplaced.iter().position(|u| u.id == id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "unplaced not found"}))).into_response();
    };
    // 分支一：挂到现有 place
    if let Some(pid) = body.get("placeId").and_then(|v| v.as_str()).filter(|s| !s.trim().is_empty()) {
        if !store.places.iter().any(|p| p.id == pid) {
            return (StatusCode::NOT_FOUND, Json(json!({"error": "place not found"}))).into_response();
        }
        let u = store.unplaced.remove(pos);
        let p = store.places.iter_mut().find(|p| p.id == pid).unwrap();
        p.photos.push(Photo { id: u.id, url: u.url, thumb: u.thumb, date: None });
        let resp = p.clone();
        save_store(&store);
        return Json(resp).into_response();
    }
    // 分支二：新建 place
    let lat = body.get("lat").and_then(|v| v.as_f64());
    let lng = body.get("lng").and_then(|v| v.as_f64());
    let name = body
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let (Some(lat), Some(lng), Some(name)) = (lat, lng, name) else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "placeId or lat/lng/name required"}))).into_response();
    };
    let status = match body.get("status").and_then(|v| v.as_str()) {
        Some("planned") => "planned".to_string(),
        _ => "visited".to_string(),
    };
    let date = body
        .get("date")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(String::from);
    let u = store.unplaced.remove(pos);
    let place = Place {
        id: format!("p_{}", uid()),
        lat,
        lng,
        short: name.clone(),
        name,
        city: String::new(),
        country: String::new(),
        em: sanitize_em(body.get("em").and_then(|v| v.as_str()).unwrap_or("📍")),
        date: date.clone(),
        rating: 0,
        tags: vec![],
        feel: String::new(),
        photos: vec![Photo { id: u.id, url: u.url, thumb: u.thumb, date: date.clone() }],
        cover: None,
        status,
        trip_id: None,
        visits: vec![],
        category: String::new(),
        color: None,
        guide: Value::Null,
    };
    let resp = place.clone();
    store.places.push(place);
    save_store(&store);
    Json(resp).into_response()
}

// DELETE /api/unplaced/:id — 移除并删除照片文件+缩略图
async fn delete_unplaced(
    State(st): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> axum::response::Response {
    if !is_owner(&st, &headers) {
        return unauthorized();
    }
    let mut store = st.store.lock().await;
    let Some(pos) = store.unplaced.iter().position(|u| u.id == id) else {
        return (StatusCode::NOT_FOUND, Json(json!({"error": "unplaced not found"}))).into_response();
    };
    let removed = store.unplaced.remove(pos);
    save_store(&store);
    drop(store);
    delete_photo_files(&removed.id);
    Json(json!({"ok": true})).into_response()
}

// ---------- 导出备份 ----------
// GET /api/export — zip 打包 store.json（根）+ photos/（相对结构），内存打包
async fn export_zip(State(st): State<AppState>, headers: HeaderMap) -> axum::response::Response {
    if !is_owner(&st, &headers) {
        return unauthorized();
    }
    let (store_json, refs) = {
        let s = st.store.lock().await;
        // 只收集 store 引用到的照片文件（相对 photos/ 的路径），孤儿文件不入包
        let mut refs: std::collections::BTreeSet<String> = Default::default();
        let mut push_url = |u: &str| {
            if let Some(rest) = u.strip_prefix("/photos/") {
                // 只收相对、干净的路径：拒绝 .. 穿越，拒绝以 / 开头的绝对路径
                // （strip_prefix 后 "/photos//etc/passwd" → "/etc/passwd"，join 绝对路径会脱离照片目录）
                if is_safe_rel(rest) {
                    refs.insert(rest.to_string());
                }
            }
        };
        for p in &s.places {
            for ph in &p.photos {
                push_url(&ph.url);
                push_url(&ph.thumb);
            }
            if let Some(c) = &p.cover {
                push_url(c);
            }
        }
        for u in &s.unplaced {
            push_url(&u.url);
            push_url(&u.thumb);
        }
        (serde_json::to_string_pretty(&*s).unwrap_or_else(|_| "{}".into()), refs)
    };
    let buf = match tokio::task::spawn_blocking(move || build_export_zip(&store_json, &refs)).await {
        Ok(Ok(b)) => b,
        _ => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "export failed"})))
                .into_response()
        }
    };
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/zip"),
            (header::CONTENT_DISPOSITION, "attachment; filename=travel-backup.zip"),
        ],
        buf,
    )
        .into_response()
}

fn build_export_zip(store_json: &str, refs: &std::collections::BTreeSet<String>) -> anyhow::Result<Vec<u8>> {
    use std::io::Write;
    use zip::write::SimpleFileOptions;
    let mut zw = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let deflate = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    // jpg 已压缩，Stored 免二次压缩
    let stored = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
    zw.start_file("store.json", deflate)?;
    zw.write_all(store_json.as_bytes())?;
    // 按 store 引用清单打包（BTreeSet 天然去重排序），磁盘上未被引用的孤儿文件不入包
    for rel in refs {
        let path = photo_dir().join(rel);
        if let Ok(bytes) = fs::read(&path) {
            zw.start_file(format!("photos/{rel}"), stored)?;
            zw.write_all(&bytes)?;
        }
    }
    let cur = zw.finish()?;
    Ok(cur.into_inner())
}

// ---------- 导入恢复 ----------
// POST /api/import — 接收 /api/export 产出的 travel-backup.zip，整包覆盖 store + 照片。
// 覆盖前把现行 store 落备份 store.pre-import.{ts}.bak.json，磁盘上多余的旧照片文件保留为孤儿（不做删除，宁冗余不丢数据）。
async fn import_zip(
    State(st): State<AppState>,
    headers: HeaderMap,
    mp: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> axum::response::Response {
    if !is_owner(&st, &headers) {
        return unauthorized();
    }
    let mut mp = match mp {
        Ok(m) => m,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error": "expected multipart"}))).into_response(),
    };
    // 取第一个非空文件字段作为备份包
    let mut zip_bytes: Option<Vec<u8>> = None;
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            _ => break,
        };
        match field.bytes().await {
            Ok(b) if !b.is_empty() => {
                zip_bytes = Some(b.to_vec());
                break;
            }
            _ => continue,
        }
    }
    let Some(bytes) = zip_bytes else {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "缺少备份文件"}))).into_response();
    };
    // zip 解析 + 校验放阻塞线程池，任何一处不合法整体拒绝、不落盘
    let parsed = tokio::task::spawn_blocking(move || parse_backup_zip(&bytes)).await;
    let (new_store, photos) = match parsed {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return (StatusCode::BAD_REQUEST, Json(json!({"error": e}))).into_response(),
        Err(_) => {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": "导入失败"}))).into_response()
        }
    };
    let photo_count = photos.iter().filter(|(rel, _)| !rel.starts_with("thumb/")).count();
    let _ = fs::copy(
        store_file(),
        data_dir().join(format!("store.pre-import.{}.bak.json", now_secs())),
    );
    fs::create_dir_all(thumb_dir()).ok();
    for (rel, data) in &photos {
        let path = photo_dir().join(rel);
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&path, data);
    }
    let place_count;
    {
        let mut store = st.store.lock().await;
        *store = new_store;
        place_count = store.places.len();
        save_store(&store);
        // 清理不再被引用的旧照片：否则旧图物理残留在 data/photos，/photos/{id}.jpg 无鉴权仍可 GET，磁盘也只增不减
        gc_orphan_photos(&store);
    }
    Json(json!({"ok": true, "places": place_count, "photos": photo_count})).into_response()
}

// 删除 data/photos 与 thumb 下不再被 store 引用的 .jpg（导入替换整库后清理孤儿）。
// 保守：只处理直接位于 photo_dir / thumb_dir 的 .jpg 文件，引用集取自 places[].photos[].id + unplaced[].id。
fn gc_orphan_photos(store: &Store) {
    let mut referenced: std::collections::HashSet<String> = std::collections::HashSet::new();
    for p in &store.places {
        for ph in &p.photos {
            referenced.insert(ph.id.clone());
        }
    }
    for u in &store.unplaced {
        referenced.insert(u.id.clone());
    }
    for dir in [photo_dir(), thumb_dir()] {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for ent in rd.flatten() {
            let path = ent.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jpg") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                if !referenced.contains(stem) {
                    let _ = fs::remove_file(&path);
                }
            }
        }
    }
}

// zip → (Store, [(photos/ 下相对路径, 内容)])：store.json 必须存在且反序列化为 Store；
// 照片路径字符白名单 + 拒绝 ".."/绝对路径，杜绝解包目录穿越
fn parse_backup_zip(bytes: &[u8]) -> Result<(Store, Vec<(String, Vec<u8>)>), String> {
    use std::io::Read;
    // zip 炸弹防护上限
    const MAX_ENTRIES: usize = 20_000;
    const MAX_FILE: u64 = 60 * 1024 * 1024; // 单文件 60MB
    const MAX_TOTAL: u64 = 4 * 1024 * 1024 * 1024; // 解压总量 4GB
    const MAX_STORE_JSON: u64 = 64 * 1024 * 1024; // store.json 64MB
    let mut za = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| "不是有效的 zip 备份包".to_string())?;
    if za.len() > MAX_ENTRIES {
        return Err("备份包条目过多，疑似异常文件".to_string());
    }
    let mut store: Option<Store> = None;
    let mut photos: Vec<(String, Vec<u8>)> = Vec::new();
    let mut total: u64 = 0;
    for i in 0..za.len() {
        let mut f = za.by_index(i).map_err(|_| "zip 读取失败".to_string())?;
        let name = f.name().to_string();
        if name.ends_with('/') {
            continue;
        }
        let declared = f.size();
        if declared > MAX_FILE {
            return Err("备份包内有超大文件，已拒绝".to_string());
        }
        total = total.saturating_add(declared);
        if total > MAX_TOTAL {
            return Err("备份包解压后体积过大，已拒绝".to_string());
        }
        if name == "store.json" {
            if declared > MAX_STORE_JSON {
                return Err("store.json 过大，疑似异常".to_string());
            }
            let mut s = String::new();
            f.read_to_string(&mut s).map_err(|_| "store.json 读取失败".to_string())?;
            store = Some(serde_json::from_str::<Store>(&s).map_err(|_| "store.json 结构不合法".to_string())?);
        } else if let Some(rel) = name.strip_prefix("photos/") {
            if rel.is_empty()
                || rel.contains("..")
                || rel.starts_with('/')
                || !rel.chars().all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c))
            {
                continue;
            }
            let mut buf = Vec::new();
            f.read_to_end(&mut buf).map_err(|_| "照片读取失败".to_string())?;
            photos.push((rel.to_string(), buf));
        }
    }
    let store = store.ok_or_else(|| "备份包里没有 store.json".to_string())?;
    Ok((store, photos))
}

// ---------- Geocode 代理 ----------
#[derive(Deserialize)]
struct GeocodeQuery {
    q: String,
}
async fn geocode(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(gq): Query<GeocodeQuery>,
) -> axum::response::Response {
    if !is_owner(&st, &headers) {
        return unauthorized();
    }
    let q = gq.q.trim();
    if q.is_empty() {
        return Json(json!([])).into_response();
    }
    let resp = st
        .http
        .get("https://nominatim.openstreetmap.org/search")
        .query(&[
            ("format", "json"),
            ("limit", "5"),
            ("accept-language", "zh"),
            ("addressdetails", "1"),
            ("q", q),
        ])
        .send()
        .await;
    let j: Value = match resp {
        Ok(r) => match r.json().await {
            Ok(j) => j,
            Err(_) => return (StatusCode::BAD_GATEWAY, Json(json!({"error": "geocode parse error"}))).into_response(),
        },
        Err(_) => return (StatusCode::BAD_GATEWAY, Json(json!({"error": "geocode fetch error"}))).into_response(),
    };
    let out: Vec<Value> = j
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let lat: f64 = item.get("lat")?.as_str()?.parse().ok()?;
                    let lng: f64 = item.get("lon")?.as_str()?.parse().ok()?;
                    let display = item.get("display_name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .unwrap_or_else(|| display.split(',').next().unwrap_or("").trim().to_string());
                    let addr = item.get("address");
                    let city = addr
                        .and_then(|a| {
                            ["city", "town", "village", "county", "state_district", "state"]
                                .iter()
                                .find_map(|k| a.get(k).and_then(|v| v.as_str()))
                        })
                        .unwrap_or("")
                        .to_string();
                    let country = addr
                        .and_then(|a| a.get("country"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    Some(json!({"name": name, "lat": lat, "lng": lng, "display": display, "city": city, "country": country}))
                })
                .collect()
        })
        .unwrap_or_default();
    Json(Value::Array(out)).into_response()
}

// ---------- 瓦片代理 ----------
fn map_mode() -> String {
    std::env::var("MAP_MODE").unwrap_or_else(|_| "raster".into())
}

async fn style(State(st): State<AppState>) -> impl IntoResponse {
    // raster 模式：轻量 CARTO voyager 栅格底图（一片 ~20-50KB，弱网友好），矢量版设 MAP_MODE=vector 切回
    if map_mode() == "raster" {
        return Json(json!({
            "version": 8,
            "name": "voyager-raster",
            "sources": {
                "carto": {
                    "type": "raster",
                    // 走自家服务器 /carto 代理（磁盘缓存+稳定送达），配合前端 SW 预装全球低层级，弱网也秒开
                    "tiles": [
                        "/carto/rastertiles/voyager/{z}/{x}/{y}@2x.png"
                    ],
                    "tileSize": 512,
                    "maxzoom": 19,
                    "attribution": "© OpenStreetMap contributors © CARTO"
                }
            },
            "layers": [
                {"id": "bg", "type": "background", "paint": {"background-color": "#e8ecef"}},
                {"id": "carto", "type": "raster", "source": "carto"}
            ]
        }))
        .into_response();
    }
    let url = format!("{}/styles/{}", OFM, ofm_style());
    let text = match st.http.get(&url).send().await {
        Ok(r) => match r.text().await {
            Ok(t) => t,
            Err(_) => return (StatusCode::BAD_GATEWAY, "style read error").into_response(),
        },
        Err(_) => return (StatusCode::BAD_GATEWAY, "style fetch error").into_response(),
    };
    let text = text.replace(&format!("{}/", OFM), "/ofm/");
    let mut v: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_GATEWAY, "style parse error").into_response(),
    };
    if let Some(layers) = v.get_mut("layers").and_then(|l| l.as_array_mut()) {
        for layer in layers.iter_mut() {
            let is_symbol = layer.get("type").and_then(|t| t.as_str()) == Some("symbol");
            if is_symbol {
                if let Some(layout) = layer.get_mut("layout") {
                    if layout.get("text-field").is_some() {
                        layout["text-field"] = chinese_text_field();
                    }
                }
            }
        }
    }
    Json(v).into_response()
}

async fn ofm(State(st): State<AppState>, Path(sub): Path<String>) -> impl IntoResponse {
    if sub.contains("..") {
        return (StatusCode::BAD_REQUEST, "bad path").into_response();
    }
    let is_binary = [".pbf", ".png", ".jpg", ".jpeg", ".webp"]
        .iter()
        .any(|e| sub.ends_with(e));
    let safe: String = sub
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._/-".contains(c) { c } else { '_' })
        .collect();
    let cache_path = cache_dir().join(&safe);

    if is_binary && cache_path.exists() {
        if let Ok(buf) = fs::read(&cache_path) {
            return ok_bytes(buf, content_type_for(&sub));
        }
    }
    let url = format!("{}/{}", OFM, sub);
    let resp = match st.http.get(&url).send().await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("proxy error: {e}")).into_response(),
    };
    if !resp.status().is_success() {
        return (StatusCode::BAD_GATEWAY, format!("upstream {}", resp.status())).into_response();
    }
    let ct = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    if ct.contains("json") {
        let t = resp.text().await.unwrap_or_default();
        let t = t.replace(&format!("{}/", OFM), "/ofm/");
        return (
            StatusCode::OK,
            [
                (header::CONTENT_TYPE, "application/json"),
                (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            ],
            t,
        )
            .into_response();
    }
    let buf = match resp.bytes().await {
        Ok(b) => b.to_vec(),
        Err(_) => return (StatusCode::BAD_GATEWAY, "read error").into_response(),
    };
    if is_binary {
        if let Some(parent) = cache_path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = fs::write(&cache_path, &buf);
        maybe_prune_tile_cache();
    }
    ok_bytes(buf, content_type_for(&sub))
}

// CARTO 栅格瓦片代理+磁盘缓存（sub 形如 rastertiles/voyager/{z}/{x}/{y}@2x.png）
async fn carto(State(st): State<AppState>, Path(sub): Path<String>) -> impl IntoResponse {
    if sub.contains("..") || !sub.ends_with(".png") {
        return (StatusCode::BAD_REQUEST, "bad path").into_response();
    }
    let safe: String = sub
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._/-@".contains(c) { c } else { '_' })
        .collect();
    let cache_path = cache_dir().join("carto").join(&safe);
    if cache_path.exists() {
        if let Ok(buf) = fs::read(&cache_path) {
            return ok_bytes(buf, "image/png");
        }
    }
    let url = format!("https://a.basemaps.cartocdn.com/{}", sub);
    let resp = match st.http.get(&url).send().await {
        Ok(r) => r,
        Err(e) => return (StatusCode::BAD_GATEWAY, format!("proxy error: {e}")).into_response(),
    };
    if !resp.status().is_success() {
        return (StatusCode::BAD_GATEWAY, format!("upstream {}", resp.status())).into_response();
    }
    let buf = match resp.bytes().await {
        Ok(b) => b.to_vec(),
        Err(_) => return (StatusCode::BAD_GATEWAY, "read error").into_response(),
    };
    if let Some(parent) = cache_path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&cache_path, &buf);
    maybe_prune_tile_cache();
    ok_bytes(buf, "image/png")
}

fn ok_bytes(buf: Vec<u8>, ct: &'static str) -> axum::response::Response {
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, ct),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            (header::CACHE_CONTROL, "public,max-age=604800"),
        ],
        buf,
    )
        .into_response()
}

// ---------- 上传 ----------
async fn upload(
    State(st): State<AppState>,
    headers: HeaderMap,
    mp: Result<Multipart, axum::extract::multipart::MultipartRejection>,
) -> axum::response::Response {
    if !is_owner(&st, &headers) {
        return unauthorized();
    }
    let mut mp = match mp {
        Ok(m) => m,
        Err(_) => return (StatusCode::BAD_REQUEST, "expected multipart").into_response(),
    };
    let mut results: Vec<Value> = Vec::new();
    let mut target_place_id: Option<String> = None; // multipart 可选字段 place_id
    fs::create_dir_all(thumb_dir()).ok();

    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(_) => break,
        };
        let field_name = field.name().map(String::from).unwrap_or_default();
        let file_name = field.file_name().map(String::from).unwrap_or_default();
        let data = match field.bytes().await {
            Ok(b) => b.to_vec(),
            Err(e) => {
                // 读字段失败（超限/断流）必须显式上报，不能静默吞——否则前端只会看到"没反应"
                results.push(json!({"error": format!("读取失败：{}", e), "name": file_name}));
                continue;
            }
        };
        if field_name == "place_id" {
            let s = String::from_utf8_lossy(&data).trim().to_string();
            if !s.is_empty() {
                target_place_id = Some(s);
            }
            continue;
        }
        if data.is_empty() {
            continue;
        }
        let id = uid();
        let orient = exif_orientation(&data);
        let gps = exif_gps(&data);
        let date = exif_date(&data);

        // 图片解码+缩放是 CPU 密集，放 spawn_blocking，避免阻塞 tokio worker 让并发请求（含只读浏览）排队。
        // 一次解码同时产出：主图 + 缩略图 + 情绪主色，避免重复 decode（HEIC 尤其贵）。
        let is_heic = looks_like_heic(&data);
        let (main_res, thumb_res, color) = {
            let d = data; // move 进阻塞任务，raw 数据后续不再需要（EXIF 已读毕）
            tokio::task::spawn_blocking(move || match decode_oriented(&d, orient) {
                Ok(img) => {
                    let color = dominant_color_of(&img);
                    let m = encode_jpeg(&img, 1600, 82);
                    let t = encode_jpeg(&img, 600, 72).ok();
                    (m, t, Some(color))
                }
                Err(e) => (Err(e), None, None),
            })
            .await
            .unwrap_or((Err(anyhow::anyhow!("图片处理线程异常")), None, None))
        };
        // 解码失败（最常见=iPhone HEIC，image crate 不支持）绝不能静默吞——
        // 否则 EXIF 仍读到 GPS/日期→照片"成功"落地图，但 /photos/{id}.jpg 从没写盘→点开 404 丢照片。
        let main = match main_res {
            Ok(m) => m,
            Err(_) => {
                let hint = if is_heic {
                    "这张 HEIC 照片无法解码（可能已损坏或为特殊子格式），请换一张或导出为 JPG 再传"
                } else {
                    "无法识别的图片格式，请改用 JPG/PNG"
                };
                results.push(json!({"error": hint, "name": file_name}));
                continue;
            }
        };
        if let Err(e) = fs::write(photo_dir().join(format!("{id}.jpg")), &main) {
            results.push(json!({"error": format!("照片写盘失败：{e}"), "name": file_name}));
            continue;
        }
        if let Some(th) = thumb_res {
            let _ = fs::write(thumb_dir().join(format!("{id}.jpg")), &th);
        }
        let photo = Photo {
            id: id.clone(),
            url: format!("/photos/{id}.jpg"),
            thumb: format!("/photos/thumb/{id}.jpg"),
            date: date.clone(),
        };

        // 带 place_id：直接追加到指定 place，跳过就近并点
        if let Some(pid) = &target_place_id {
            let mut store = st.store.lock().await;
            if let Some(p) = store.places.iter_mut().find(|p| &p.id == pid) {
                p.photos.push(photo.clone());
                if p.date.is_none() {
                    p.date = date.clone();
                }
                if p.color.is_none() {
                    p.color = color.clone();
                }
                let place_clone = p.clone();
                save_store(&store); // 增量落盘，防中途崩溃丢照片索引
                results.push(json!({"photo": photo, "place": place_clone}));
                continue;
            }
            // place 不存在则回落到就近并点逻辑
        }

        let (lat, lng) = match gps {
            Some(v) => v,
            None => {
                // 读不到 GPS：照片已压缩存盘，进 unplaced 暂存区等待手动归位
                {
                    let mut store = st.store.lock().await;
                    store.unplaced.push(UnplacedPhoto {
                        id: id.clone(),
                        url: photo.url.clone(),
                        thumb: photo.thumb.clone(),
                        name: file_name.clone(),
                    });
                    save_store(&store); // 增量落盘，防中途崩溃丢 unplaced 索引
                }
                results.push(json!({"photo": photo, "needLocation": true, "unplacedId": id}));
                continue;
            }
        };

        // 先在锁外做反查（网络请求），拿到 city/region 再进锁；建点时再复查一次就近点，
        // 关掉"检查→创建"跨锁的 TOCTOU（多标签页同城并发上传不再产生重复地点/旅程）。
        let geo = reverse_geocode(&st.http, lat, lng).await; // 不持锁
        let (city, name, country, region) =
            geo.unwrap_or_else(|| (String::new(), "新地点".into(), String::new(), String::new()));
        let place_clone = {
            let mut store = st.store.lock().await;
            let existing = store
                .places
                .iter()
                .position(|p| (p.lat - lat).abs() < 0.0015 && (p.lng - lng).abs() < 0.0015);
            let pid = if let Some(i) = existing {
                store.places[i].photos.push(photo.clone());
                if store.places[i].date.is_none() {
                    store.places[i].date = date.clone();
                }
                if store.places[i].color.is_none() {
                    store.places[i].color = color.clone();
                }
                store.places[i].id.clone()
            } else {
                // 按行政大区自动归程（东京各区归到"东京"，不再碎成 ward 级旅程）
                let group = trip_group_name(&region, &city);
                let trip_id = if !group.is_empty() {
                    if let Some(t) = store.trips.iter().find(|t| t.name == group) {
                        Some(t.id.clone())
                    } else {
                        const PALETTE: &[&str] =
                            &["#E0684F", "#4F7DA3", "#3E6B57", "#C89B4B", "#8A6BA8", "#4FA39A"];
                        let color = PALETTE[store.trips.len() % PALETTE.len()].to_string();
                        let tid = format!("t_{}", uid());
                        store.trips.push(Trip {
                            id: tid.clone(),
                            name: group.clone(),
                            em: "🧳".into(),
                            color,
                            start_date: String::new(),
                            end_date: String::new(),
                            note: String::new(),
                            status: "done".into(),
                            checklist: vec![],
                            guide: Value::Null,
                        });
                        Some(tid)
                    }
                } else {
                    None
                };
                let place = Place {
                    id: format!("p_{id}"),
                    lng,
                    lat,
                    name: name.clone(),
                    short: if !name.is_empty() { name.clone() } else { city.clone() },
                    city: city.clone(),
                    country: country.clone(),
                    em: "📍".into(),
                    date: date.clone(),
                    rating: 0,
                    tags: vec![],
                    feel: String::new(),
                    photos: vec![photo.clone()],
                    cover: None,
                    status: "visited".into(),
                    trip_id,
                    visits: vec![],
                    category: String::new(),
                    color: color.clone(), // 情绪色卡：新建点用首张照片主色
                    guide: Value::Null,
                };
                let pid = place.id.clone();
                store.places.push(place);
                pid
            };
            // 增量落盘：每张处理完即 save，进程中途崩溃不会丢已入库的照片索引（防孤儿）
            save_store(&store);
            store.places.iter().find(|p| p.id == pid).cloned()
        };
        results.push(json!({"photo": photo, "place": place_clone}));
    }

    let places_snapshot = {
        let store = st.store.lock().await;
        save_store(&store);
        store.places.clone()
    };
    Json(json!({"ok": true, "results": results, "places": places_snapshot})).into_response()
}

// 情绪色卡回填：给已有但无 color 的地点，从其封面/首张照片文件计算主色，落盘一次。
fn backfill_colors() {
    let mut store = load_store();
    let mut changed = 0;
    for p in store.places.iter_mut() {
        if p.color.is_some() {
            continue;
        }
        let file = p
            .cover
            .as_deref()
            .or_else(|| p.photos.first().map(|ph| ph.url.as_str()));
        let Some(url) = file else { continue };
        // url 形如 /photos/{id}.jpg → 取磁盘缩略图（小、快）
        let stem = url.rsplit('/').next().unwrap_or("");
        let path = thumb_dir().join(stem);
        let path = if path.exists() { path } else { photo_dir().join(stem) };
        if let Ok(bytes) = fs::read(&path) {
            if let Ok(img) = image::load_from_memory(&bytes) {
                p.color = Some(dominant_color_of(&img));
                changed += 1;
            }
        }
    }
    if changed > 0 {
        eprintln!("[backfill_colors] 为 {changed} 个地点补充了情绪主色");
        save_store(&store);
    }
}
#[tokio::main]
async fn main() {
    for d in [cache_dir(), thumb_dir()] {
        let _ = fs::create_dir_all(d);
    }
    migrate_store(); // v1 → v2 迁移（幂等，先备份 store.v1.bak.json）
    backfill_colors(); // 情绪色卡：给存量无 color 的地点补主色（一次性，幂等）
    let state = AppState {
        store: Arc::new(Mutex::new(load_store())),
        sessions: Arc::new(std::sync::Mutex::new(load_sessions())),
        http: reqwest::Client::builder()
            .user_agent("travel-map-app/1.0")
            .timeout(Duration::from_secs(10)) // Nominatim 挂起时不再无限 pending 卡住上传
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap(),
        login_guard: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        like_guard: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        msg_guard: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
    };

    let app = Router::new()
        .route("/", get(index_html))
        .route("/index.html", get(index_html))
        .route("/api/health", get(health))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/me", get(me))
        .route("/api/places", get(places).post(create_place))
        .route("/api/profile", patch(patch_profile))
        .route("/api/places/:id", patch(patch_place).delete(delete_place))
        .route("/api/places/:id/photos/:photo_id", delete(delete_place_photo))
        .route("/api/trips", post(create_trip))
        .route("/api/trips/:id", patch(patch_trip).delete(delete_trip))
        .route("/api/geocode", get(geocode))
        .route("/api/upload", post(upload))
        .route("/api/unplaced", get(list_unplaced))
        .route("/api/unplaced/:id", delete(delete_unplaced))
        .route("/api/unplaced/:id/assign", post(assign_unplaced))
        .route("/api/export", get(export_zip))
        .route("/api/import", post(import_zip))
        .route("/api/places/:id/like", post(like_place))
        .route("/api/messages", get(list_messages).post(post_message))
        .route("/api/messages/:id", delete(delete_message))
        .route("/api/view", post(track_view))
        .route("/p/:id", get(share_place_page))
        .route("/t/:id", get(share_trip_page))
        .route("/tiles/style.json", get(style))
        .route("/ofm/*sub", get(ofm))
        .route("/carto/*sub", get(carto))
        .nest_service("/photos", ServeDir::new("data/photos"))
        .fallback_service(ServeDir::new("public"))
        .layer(tower_http::compression::CompressionLayer::new())
        // 手机原图 5-15MB、批量上传可达百MB；axum 默认 2MB 会把超限字段静默吞掉（表现为"上传没反应"）
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(state);

    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("travel-map-app on http://{addr}  (style: {})", ofm_style());
    axum::serve(listener, app).await.unwrap();
}

// 内置样例数据
const SEED: &str = r#"{
  "places": [
    {"id":"shinjuku","name":"新宿 · 歌舞伎町","short":"新宿","em":"🌃","city":"东京","country":"日本","lng":139.7004,"lat":35.6938,"date":"2026.06.17","rating":4,"tags":["夜景","繁华","哥斯拉头"],"photos":[],"feel":"住在新宿华盛顿，晚上去歌舞伎町溜达。霓虹密得晃眼。"},
    {"id":"asakusa","name":"浅草寺 · 雷门","short":"浅草寺","em":"⛩️","city":"东京","country":"日本","lng":139.7967,"lat":35.7148,"date":"2026.06.18","rating":5,"tags":["寺庙","老街","人形烧"],"photos":[],"feel":"雷门那盏大红灯笼比想象中震撼。仲见世通一路小吃。"},
    {"id":"kamakura","name":"镰仓 · 高德院大佛","short":"镰仓大佛","em":"🗿","city":"镰仓","country":"日本","lng":139.5359,"lat":35.3169,"date":"2026.06.19","rating":5,"tags":["海边","江之电","古都"],"photos":[],"feel":"坐江之电一路晃到海边。高德院的大佛安安静静坐了八百年。"},
    {"id":"shibuya","name":"涩谷 · 十字路口","short":"涩谷","em":"🚦","city":"东京","country":"日本","lng":139.7016,"lat":35.6580,"date":"2026.06.17","rating":4,"tags":["潮流","人潮"],"photos":[],"feel":"传说中全世界最忙的路口，绿灯一亮人从四面八方涌出来。"},
    {"id":"odaiba","name":"台场 · 海滨","short":"台场","em":"🎡","city":"东京","country":"日本","lng":139.7766,"lat":35.6297,"date":"2026.06.20","rating":3,"tags":["海湾","高达"],"photos":[],"feel":"冲着等身高达来的，晚上变身表演有点燃。"},
    {"id":"ginza","name":"银座 · 中央通","short":"银座","em":"🛍️","city":"东京","country":"日本","lng":139.7637,"lat":35.6717,"date":"2026.06.20","rating":4,"tags":["购物","咖啡"],"photos":[],"feel":"金子眼镜配了副新的。周末中央通步行街，闲逛很舒服。"}
  ],
  "cities": [
    {"nm":"横滨","lng":139.638,"lat":35.444},{"nm":"川崎","lng":139.703,"lat":35.531},
    {"nm":"千叶","lng":140.123,"lat":35.607},{"nm":"埼玉","lng":139.649,"lat":35.861},
    {"nm":"横须贺","lng":139.672,"lat":35.281}
  ]
}"#;
