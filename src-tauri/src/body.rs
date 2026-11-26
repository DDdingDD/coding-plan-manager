//! 消息体落库编码与前缀增量候选缓存。
//!
//! messages.request_body/response_body 沿用原列名，编码后存 BLOB（旧库为明文 TEXT，
//! 读取时按存储类区分）。BLOB = 1 字节标签 + 负载：
//! - `TAG_ZSTD`：u32 LE 原始长度 + zstd 帧（帧尾带校验和；长度用于精确分配，
//!   兼作损坏校验）
//! - `TAG_RAW`：原始字节——zstd 不比原长短或压缩失败时的兜底，编码永不失败
//!
//! 空体存空 BLOB。
//!
//! 前缀增量：同一会话每轮重发全部历史，相邻请求体是前缀关系。新行只存相对某条
//! 近期同聚合器请求体的新增后缀，前缀长度与来源行 id 记在 req_prefix_len/req_ref_id。
//! `BodyStore` 是进程内的近期请求体环，只用于写入时选候选；读取只依赖库内数据。

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

pub const TAG_ZSTD: u8 = 0x01;
pub const TAG_RAW: u8 = 0x02;

/// 在线路（代理落库路径）压缩级别：压缩耗时直接落在请求收尾的持锁区间内
pub const ONLINE_LEVEL: i32 = 3;

/// 构成增量的最小公共前缀（绝对阈值），太小不值得引入一条引用依赖
pub const MIN_PREFIX: usize = 16 * 1024;
/// 公共前缀还须不低于 body 的 1/MIN_PREFIX_RATIO
pub const MIN_PREFIX_RATIO: usize = 4;
/// 引用链最大深度：来源已在最大深度的行不再被接续（此时存整条、另起新链），
/// 保证读取时的回溯有界
pub const MAX_DEPTH: u8 = 32;

/// 单条解码后的体积上限：落库体上限 2MB，留足余量。超限说明长度前缀已损坏，
/// 拒绝分配（panic=abort 下分配失败会直接杀进程）
const MAX_DECODED: usize = 64 << 20;

/// 每个聚合器环最多保留的条数 / 字节数
const RING_ENTRIES: usize = 128;
const RING_BYTES: usize = 64 << 20;
/// 所有环合计的字节上限：超出则从最久未用的环开始淘汰
const TOTAL_BYTES: usize = 128 << 20;
/// 最多保留多少个聚合器的环（LRU）
const MAX_RINGS: usize = 8;

/// 候选粗排的探测偏移：先比少量前缀定位最像的候选，再做精确 LCP
const PROBES: [usize; 4] = [4 * 1024, 16 * 1024, 64 * 1024, 256 * 1024];
/// 未过这一级探测的候选 LCP 必然小于 MIN_PREFIX，直接排除
const MIN_PROBE_LEVEL: usize = 2;
/// 最多对几个候选做精确 LCP（同级中取最新的）
const MAX_EXACT: usize = 4;

/// 落库编码器：持有可复用的 zstd 压缩上下文（回填成批编码时复用，免得每行重建工作区）。
/// 上下文创建失败的编码器一律输出原始字节，编码永不失败
pub struct Encoder {
    comp: Option<zstd::bulk::Compressor<'static>>,
}

impl Encoder {
    pub fn new(level: i32) -> Self {
        let comp = zstd::bulk::Compressor::new(level).ok().and_then(|mut c| {
            // 帧尾校验和：损坏的数据解码时报错，而不是解出等长的垃圾
            c.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))
                .ok()?;
            Some(c)
        });
        Self { comp }
    }

    /// 编码一条 body 用于落库（格式见模块文档）
    pub fn encode(&mut self, data: &[u8]) -> Vec<u8> {
        if data.is_empty() {
            return Vec::new();
        }
        if let Some(z) = self.comp.as_mut().and_then(|c| c.compress(data).ok()) {
            // 压缩形态的固定开销：标签 1 字节 + 长度前缀 4 字节
            if z.len() + 5 < data.len() {
                let mut out = Vec::with_capacity(z.len() + 5);
                out.push(TAG_ZSTD);
                out.extend_from_slice(&(data.len() as u32).to_le_bytes());
                out.extend_from_slice(&z);
                return out;
            }
        }
        let mut out = Vec::with_capacity(data.len() + 1);
        out.push(TAG_RAW);
        out.extend_from_slice(data);
        out
    }
}

/// 单次编码（在线落库路径用）
pub fn encode(data: &[u8], level: i32) -> Vec<u8> {
    Encoder::new(level).encode(data)
}

/// 解码落库的 body；数据损坏时返回中文错误，不 panic
pub fn decode(blob: &[u8]) -> Result<Vec<u8>, String> {
    if blob.is_empty() {
        return Ok(Vec::new());
    }
    match blob[0] {
        TAG_RAW => Ok(blob[1..].to_vec()),
        TAG_ZSTD => {
            let Some(len_bytes) = blob.get(1..5) else {
                return Err("请求/响应体数据损坏：缺少长度前缀".into());
            };
            let len = u32::from_le_bytes([len_bytes[0], len_bytes[1], len_bytes[2], len_bytes[3]])
                as usize;
            if len > MAX_DECODED {
                return Err(format!("请求/响应体数据损坏：长度前缀异常（{len} 字节）"));
            }
            // capacity 即解压上限：按记录长度精确分配，多出 1 字节都会报错而非扩容
            let out = zstd::bulk::decompress(&blob[5..], len)
                .map_err(|e| format!("请求/响应体解压失败：{e}"))?;
            if out.len() != len {
                return Err(format!(
                    "请求/响应体数据损坏：长度不符（记录 {len}，实际 {}）",
                    out.len()
                ));
            }
            Ok(out)
        }
        other => Err(format!("请求/响应体数据损坏：未知编码标签 0x{other:02x}")),
    }
}

/// 最长公共前缀长度（字节）。先 64 字节步进、再 8 字节、最后逐字节
pub fn lcp(a: &[u8], b: &[u8]) -> usize {
    let n = a.len().min(b.len());
    let mut i = 0;
    while i + 64 <= n && a[i..i + 64] == b[i..i + 64] {
        i += 64;
    }
    while i + 8 <= n && a[i..i + 8] == b[i..i + 8] {
        i += 8;
    }
    while i < n && a[i] == b[i] {
        i += 1;
    }
    i
}

/// body 与候选连续通过的探测级数（任一级不等即停）
fn probe_level(body: &[u8], cand: &[u8]) -> usize {
    let limit = body.len().min(cand.len());
    let mut level = 0;
    for p in PROBES {
        if p > limit || body[..p] != cand[..p] {
            break;
        }
        level += 1;
    }
    level
}

/// 增量编码方案：前缀取自 ref_id 行的请求体，本行只存后缀
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Delta {
    pub ref_id: i64,
    pub prefix_len: usize,
    /// 本行在引用链中的深度（整条行为 0）
    pub depth: u8,
}

struct Entry {
    id: i64,
    /// 该行完整请求体字节（落库表示）
    bytes: Arc<[u8]>,
    depth: u8,
}

#[derive(Default)]
struct Ring {
    entries: VecDeque<Entry>,
    bytes: usize,
    last_used: u64,
}

#[derive(Default)]
struct Inner {
    rings: HashMap<i64, Ring>,
    /// 各环字节数之和，用于合计上限
    total_bytes: usize,
    /// 访问时钟，用于 LRU 淘汰
    clock: u64,
}

impl Inner {
    fn evict_oldest(&mut self) -> bool {
        // 最久未用、且仍有内容的环
        let Some(k) = self
            .rings
            .iter()
            .filter(|(_, r)| !r.entries.is_empty())
            .min_by_key(|(_, r)| r.last_used)
            .map(|(k, _)| *k)
        else {
            return false;
        };
        let Some(r) = self.rings.get_mut(&k) else {
            return false;
        };
        let Some(e) = r.entries.pop_front() else {
            return false;
        };
        r.bytes -= e.bytes.len();
        self.total_bytes -= e.bytes.len();
        true
    }
}

/// 近期请求体缓存：按聚合器分环，供落库时挑选前缀来源。
///
/// 必须在持有数据库锁期间调用（锁序恒为 DB → BodyStore）：这样「清空消息删除行」
/// 与「新行选候选」不可能交错，来源行在落库瞬间必然存在。候选只在同聚合器内选，
/// 因此按聚合器清空消息删掉的一定是完整的引用闭包。
#[derive(Default)]
pub struct BodyStore {
    inner: Mutex<Inner>,
}

impl BodyStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // panic=abort 下不会中毒，仍按既有惯例恢复而非 unwrap
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// 为 body 挑选前缀来源：返回满足阈值的最佳增量方案，没有则 None（存整条）。
    /// 调用方须自行确认来源行仍在库中
    pub fn plan_delta(&self, aggregator_id: i64, body: &[u8]) -> Option<Delta> {
        if body.len() < MIN_PREFIX {
            return None;
        }
        let inner = self.lock();
        let ring = inner.rings.get(&aggregator_id)?;
        // 自新到旧粗排，只保留通过级数最高的最新几个候选
        let mut best_level = 0;
        let mut cands: Vec<&Entry> = Vec::with_capacity(MAX_EXACT);
        for e in ring.entries.iter().rev() {
            let level = probe_level(body, &e.bytes);
            if level < MIN_PROBE_LEVEL {
                continue;
            }
            if level > best_level {
                best_level = level;
                cands.clear();
            }
            if level == best_level && cands.len() < MAX_EXACT {
                cands.push(e);
            }
        }
        let mut best: Option<Delta> = None;
        for e in cands {
            if e.depth >= MAX_DEPTH {
                continue;
            }
            let lcp = lcp(body, &e.bytes);
            if lcp < MIN_PREFIX || lcp.saturating_mul(MIN_PREFIX_RATIO) < body.len() {
                continue;
            }
            if best.map_or(true, |b| lcp > b.prefix_len) {
                best = Some(Delta {
                    ref_id: e.id,
                    prefix_len: lcp,
                    depth: e.depth + 1,
                });
            }
        }
        best
    }

    /// 落库成功后登记本行的完整请求体，供后续行选作前缀来源。
    /// 不足 MIN_PREFIX 的体不可能成为合格来源，不入环
    pub fn push(&self, aggregator_id: i64, id: i64, bytes: Arc<[u8]>, depth: u8) {
        if bytes.len() < MIN_PREFIX {
            return;
        }
        // 经 DerefMut 访问字段不能拆分借用，先取出内部引用
        let inner = &mut *self.lock();
        inner.clock = inner.clock.wrapping_add(1);
        let clock = inner.clock;
        let ring = inner.rings.entry(aggregator_id).or_default();
        ring.last_used = clock;
        ring.bytes += bytes.len();
        inner.total_bytes += bytes.len();
        ring.entries.push_back(Entry { id, bytes, depth });

        // 本环超限：弹最旧
        while ring.entries.len() > RING_ENTRIES || ring.bytes > RING_BYTES {
            let Some(e) = ring.entries.pop_front() else {
                break;
            };
            ring.bytes -= e.bytes.len();
            inner.total_bytes -= e.bytes.len();
        }

        // 合计超限：从最久未用的环开始弹（刚访问过的环 last_used 最大，排最后）
        while inner.total_bytes > TOTAL_BYTES && inner.evict_oldest() {}

        // 环数超限：淘汰最久未用的聚合器
        while inner.rings.len() > MAX_RINGS {
            let victim = inner
                .rings
                .iter()
                .min_by_key(|(_, r)| r.last_used)
                .map(|(k, _)| *k);
            match victim {
                Some(k) => {
                    if let Some(r) = inner.rings.remove(&k) {
                        inner.total_bytes -= r.bytes;
                    }
                }
                None => break,
            }
        }
    }

    /// 某聚合器的消息被整体清空（或聚合器被删除）后丢弃其环，
    /// 避免后续行引用已删除的行
    pub fn remove_aggregator(&self, aggregator_id: i64) {
        let mut inner = self.lock();
        if let Some(r) = inner.rings.remove(&aggregator_id) {
            inner.total_bytes -= r.bytes;
        }
    }

    /// 全部消息被清空后丢弃所有环
    pub fn clear(&self) {
        let mut inner = self.lock();
        inner.rings.clear();
        inner.total_bytes = 0;
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// 可压缩的伪正文（字母循环，周期 26），seed 决定相位
    fn big(len: usize, seed: u8) -> Vec<u8> {
        (0..len)
            .map(|i| b'a' + ((i as u8).wrapping_mul(7).wrapping_add(seed)) % 26)
            .collect()
    }

    fn joined(prefix: &[u8], tail: &str) -> Vec<u8> {
        let mut v = prefix.to_vec();
        v.extend_from_slice(tail.as_bytes());
        v
    }

    fn blob_of(data: &[u8]) -> Vec<u8> {
        Encoder::new(ONLINE_LEVEL).encode(data)
    }

    #[test]
    fn encode_decode_roundtrip_tags() {
        let text = format!(
            r#"{{"model":"claude-sonnet-5","messages":[{{"role":"user","content":"{}"}}]}}"#,
            "请帮我修复这个测试。".repeat(20)
        );
        let z = blob_of(text.as_bytes());
        assert_eq!(z[0], TAG_ZSTD);
        assert_eq!(u32::from_le_bytes([z[1], z[2], z[3], z[4]]) as usize, text.len());
        assert!(z.len() < text.len() / 2);
        assert_eq!(decode(&z).unwrap(), text.as_bytes());

        // 极短且不可压缩 → 原始字节标签
        let raw = vec![0xF0, 0x11, 0x02, 0x93, 0x07];
        let r = blob_of(&raw);
        assert_eq!(r[0], TAG_RAW);
        assert_eq!(decode(&r).unwrap(), raw);

        // 空体 → 空 BLOB
        assert!(encode(&[], ONLINE_LEVEL).is_empty());
        assert_eq!(decode(&[]).unwrap(), Vec::<u8>::new());

        // 非法压缩级别（低于 zstd 最小级别 -131072）：上下文建不出来，
        // 仍退回原始字节而不失败
        assert_eq!(Encoder::new(-200_000).encode(text.as_bytes())[0], TAG_RAW);

        // 大体积（落库上限量级）往返，上下文复用时结果同样正确
        let mut enc = Encoder::new(ONLINE_LEVEL);
        for seed in 3..6 {
            let large = big(2 * 1024 * 1024, seed);
            let blob = enc.encode(&large);
            assert_eq!(blob[0], TAG_ZSTD);
            assert_eq!(decode(&blob).unwrap(), large);
        }
    }

    #[test]
    fn decode_rejects_corrupt() {
        // RAW 永远可解；空 BLOB 即空体
        assert_eq!(decode(&[TAG_RAW, 1, 2, 3]).unwrap(), vec![1, 2, 3]);
        assert_eq!(decode(&[TAG_RAW]).unwrap(), Vec::<u8>::new());
        assert_eq!(decode(&[]).unwrap(), Vec::<u8>::new());
        // 记录长度为 0 的 zstd 帧
        let mut empty = vec![TAG_ZSTD];
        empty.extend_from_slice(&0u32.to_le_bytes());
        empty.extend_from_slice(&zstd::bulk::compress(&[], ONLINE_LEVEL).unwrap());
        assert_eq!(decode(&empty).unwrap(), Vec::<u8>::new());

        let text = big(64 * 1024, 0);
        let good = blob_of(&text);
        let err = |v: &[u8]| decode(v).unwrap_err();

        assert!(err(&[0x7F, 1, 2, 3]).contains("未知编码标签"));
        assert!(err(&[TAG_ZSTD]).contains("长度前缀"));
        assert!(err(&[TAG_ZSTD, 1]).contains("长度前缀"));

        // 长度前缀离谱：拒绝分配
        let mut huge = good.clone();
        huge[1..5].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(err(&huge).contains("长度前缀异常"));

        // 长度前缀偏大 / 偏小：一律报错，不越界、不截断
        for delta in [-1i8, 1, -100, 100] {
            let mut bad = good.clone();
            bad[1] = (bad[1] as i8).wrapping_add(delta) as u8;
            assert!(err(&bad).starts_with("请求/响应体"), "长度偏差 {delta}");
        }

        // 帧被截断
        let mut cut = good.clone();
        cut.truncate(good.len() / 2);
        assert!(err(&cut).starts_with("请求/响应体"));

        // 帧体损坏：哪怕改动只有 1 位，也必须报错而不是解出垃圾
        for pos in [9, good.len() / 3, good.len() - 1] {
            let mut flipped = good.clone();
            flipped[pos] ^= 0x01;
            assert!(err(&flipped).starts_with("请求/响应体"), "改动偏移 {pos}");
        }
    }

    #[test]
    fn lcp_edges() {
        assert_eq!(lcp(b"", b"abc"), 0);
        assert_eq!(lcp(b"abc", b"abc"), 3);
        assert_eq!(lcp(b"abcd", b"abce"), 3);
        // 多字节字符中间也可以是分界（按字节计，不要求字符边界）
        assert_eq!(lcp("中文abc".as_bytes(), "中文abd".as_bytes()), "中文ab".len());
        let a = big(100_000, 0);
        let mut b = a.clone();
        b.push(b'x');
        assert_eq!(lcp(&a, &b), a.len());
        // 跨 64/8 字节步进边界
        for pos in [0, 1, 6, 7, 63, 64, 65, 70, 71, 127, 128, 129, 1000] {
            let mut c = a.clone();
            c[pos] ^= 1;
            assert_eq!(lcp(&a, &c), pos, "改动偏移 {pos}");
        }
    }

    #[test]
    fn store_prefers_same_aggregator_longest_prefix() {
        let store = BodyStore::new();
        let prefix = big(64 * 1024, 0);
        let a1 = joined(&prefix, "-a1-tail");
        store.push(1, 101, Arc::from(a1.as_slice()), 0);

        let a2 = joined(&prefix, "-a2-tail");
        // 公共前缀止于两体开始分歧处（"-a" 之后），不会超过来源行长度
        assert_eq!(
            store.plan_delta(1, &a2).unwrap(),
            Delta {
                ref_id: 101,
                prefix_len: prefix.len() + "-a".len(),
                depth: 1
            }
        );
        // 另一聚合器即使前缀完全相同也不建引用
        assert!(store.plan_delta(2, &a2).is_none());
        // 不足 MIN_PREFIX 的体不选候选……
        assert!(store.plan_delta(1, &big(1024, 0)).is_none());
        // ……被 push 时也不入环
        store.push(1, 102, Arc::from(big(1024, 0).as_slice()), 0);
        assert_eq!(store.lock().rings[&1].entries.len(), 1);

        // 多个候选中取公共前缀最长的：较新但前缀更短的候选不入选
        let shorter = joined(&prefix[..48 * 1024], "-shorter");
        store.push(1, 103, Arc::from(shorter.as_slice()), 0);
        assert_eq!(store.plan_delta(1, &a2).unwrap().ref_id, 101);

        store.remove_aggregator(1);
        assert!(store.lock().rings.is_empty());
        assert!(store.plan_delta(1, &a2).is_none());
    }

    #[test]
    fn store_thresholds_and_depth_bound() {
        let store = BodyStore::new();

        // 公共前缀不足 16KB：不建增量
        let short = big(8 * 1024, 0);
        store.push(1, 1, Arc::from(joined(&short, &"x".repeat(64 * 1024)).as_slice()), 0);
        assert!(store.plan_delta(1, &joined(&short, &"y".repeat(64 * 1024))).is_none());

        // 公共前缀占比过低（20KB / 500KB）：不建增量
        let p = big(20 * 1024, 1);
        store.push(2, 1, Arc::from(joined(&p, &"x".repeat(480 * 1024)).as_slice()), 0);
        assert!(store
            .plan_delta(2, &joined(&p, &"y".repeat(480 * 1024)))
            .is_none());

        // 来源已在最大深度：不再接续
        let base = big(64 * 1024, 2);
        store.push(3, 1, Arc::from(joined(&base, "-root").as_slice()), MAX_DEPTH);
        assert!(store.plan_delta(3, &joined(&base, "-next")).is_none());

        // 逐轮增长的会话：深度 = 来源深度 + 1，且永不超过 MAX_DEPTH
        store.clear();
        let mut deltas = 0;
        let mut depths: HashMap<i64, u8> = HashMap::new();
        for i in 0..300 {
            let id = 1000 + i;
            let body = joined(&base, &format!("-turn-{i}"));
            let depth = store.plan_delta(3, &body).map_or(0, |d| {
                assert!(d.depth <= MAX_DEPTH, "第 {i} 行深度 {} 越界", d.depth);
                assert_eq!(d.depth, depths[&d.ref_id] + 1, "第 {i} 行深度应为其来源 +1");
                d.depth
            });
            if depth > 0 {
                deltas += 1;
            }
            depths.insert(id, depth);
            store.push(3, id, Arc::from(body.as_slice()), depth);
        }
        assert!(deltas > 250, "绝大部分行应存成增量，实际 {deltas}/300");
    }

    #[test]
    fn store_ring_eviction_and_clear() {
        let store = BodyStore::new();
        let mut first = Vec::new();
        for i in 0..=RING_ENTRIES {
            // 首字节各不相同：不同条目之间过不了第一级探测
            let body = joined(&[b'A' + i as u8], &"z".repeat(20 * 1024));
            if i == 0 {
                first = body.clone();
            }
            store.push(1, i as i64, Arc::from(body.as_slice()), 0);
        }
        // 环容 RING_ENTRIES，最旧的一条已被淘汰
        assert!(store.plan_delta(1, &first).is_none());
        let last = joined(&[b'A' + RING_ENTRIES as u8], &"z".repeat(20 * 1024));
        assert_eq!(store.plan_delta(1, &last).unwrap().ref_id, RING_ENTRIES as i64);
        // 字节计账与环内实际一致
        {
            let inner = store.lock();
            let sum: usize = inner.rings.values().map(|r| r.bytes).sum();
            assert_eq!(inner.total_bytes, sum);
        }

        // 清空只影响指定聚合器
        store.push(2, 500, Arc::from(big(32 * 1024, 5).as_slice()), 0);
        store.remove_aggregator(1);
        assert!(store.plan_delta(1, &last).is_none());
        assert!(store.plan_delta(2, &big(32 * 1024, 5)).is_some());
        store.clear();
        assert!(store.plan_delta(2, &big(32 * 1024, 5)).is_none());
        assert_eq!(store.lock().total_bytes, 0);
    }
}
