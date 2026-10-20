#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 中兴光猫控制面板：Telnet 图形客户端（egui/eframe）。
//!
//! 支持多机型：连上设备后自动识别（见 `model.rs`），
//! 并加载该机型对应的命令清单（见 `presets.rs`）。
//!
//! 协议实现在 `telnet.rs`，由 lib.rs 以 `pub mod telnet` 暴露，
//! 这样 `tests/` 下的端到端测试可以直接驱动它连假光猫。

mod theme;
mod ui;

use eframe::egui;
use std::sync::mpsc::{channel, Receiver, Sender};
use zte_g7615_panel::telnet::{ErrKind, Telnet};

use zte_g7615_panel::dbcli;

/// 日志上限：sendcmd 大输出多次执行后会涨到很大，
/// 而每帧都要对整段文本重新排版，不限制会明显卡顿。
const LOG_MAX_BYTES: usize = 256 * 1024;

/// 单个会话日志文件的字节上限。
///
/// 内存里的日志裁到 LOG_MAX_BYTES，但**落盘的那一份没有上限** ——
/// 一次长会话刷几十遍整表回读，单个文件能涨到几十 MB，
/// 而旧日志的清理只在下次启动时发生。超限就轮转一份 .old.txt：
/// 当前文件从空开始，上一份还能查。
const LOG_FILE_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// 日志文件超过上限时轮转：现文件改名为 <名>.old.txt（覆盖旧的），
/// 下次 append 会重新创建当前文件。
fn rotate_log_file(path: &std::path::Path) -> std::io::Result<()> {
    let old = path.with_extension("old.txt");
    // Windows 的 rename 带 REPLACE_EXISTING，与 Unix 语义一致：
    // 目标已存在时直接覆盖，所以不必先删。
    std::fs::rename(path, &old)
}

/// 当前日志文件是否已经该轮转了。
///
/// 单独成函数是为了能被单测钉住阈值：写死成 true（每行都轮转）
/// 或 false（永不轮转）都不会有任何编译错误，只会让文件无限长大。
fn should_rotate_log(bytes: u64) -> bool {
    bytes > LOG_FILE_MAX_BYTES
}

// ---------------- 消息 ----------------
enum ToWorker {
    Connect { host: String, port: String, user: String, pwd: String },
    Disconnect,
    Run { title: String, cmds: Vec<String> },
    /// 单条自定义命令，不走分组预设
    RunRaw { cmd: String },
    /// 清空回读校验卡片。
    ///
    /// 之所以绕一圈让 worker 去发 `ToUi::VerifyCleared`：
    /// 界面直接 `clear()` 与 worker 正在发送的 `Verify` 是两条并发路径，
    /// 用户点完「清除」，卡片上会立刻又冒出一条旧结论。
    /// 走同一个通道，两条消息就有了先后。
    ClearVerifications,
    /// 刷机级闸门的「① 备份并检查」。
    ///
    /// 为什么不复用 `Run`：`Run` 的备份是**顺带**做的，只对
    /// `sendcmd 1 DB set` 推导得出表名，而且备份失败也不阻断。
    /// 改区域这种不可逆操作不能接受「顺带」—— 必须先把原值读全、
    /// 写盘成功，才允许闸门进入下一步。所以单独一条消息，
    /// 由 worker 明确回报成功与否。
    CriticalBackup {
        /// 本次备份的序号，原样回传。
        ///
        /// 闸门可以被「取消」关掉再重新打开，而 worker 那条备份还在跑。
        /// 不带序号时，上一轮的 `CriticalBackupDone` 会落到**新表单**上，
        /// 把它填成「已备份」，第 ① 步在当前这次打开里从没执行过却解锁了 ②③。
        token: u64,
        title: String,
        /// 要备份原值的表名
        tables: Vec<String>,
        /// 顺带要跑的只读探测（当前区域码 / 串码等）
        probe_cmds: Vec<String>,
        /// 即将执行、要一并写进备份文件的命令原文
        planned_cmds: Vec<String>,
    },
}

enum ToUi {
    Log(String),
    Status(bool),
    Connecting(bool),
    Busy(bool),
    /// 写后回读校验的结果：命令原文 + 三档结论。
    /// 单独一条消息而不是拼进日志，是为了让 UI 能把它高亮出来
    /// （静默失败最危险的地方就是它混在满屏日志里看不见）。
    Verify { cmd: String, result: VerifyResult, detail: String },
    /// 机型识别结果：机型 + 该机型下被剔除的命令 + 提示文本。
    ///
    /// 识别出来后界面要换一套按钮，所以必须单独通知 UI，
    /// 而不是只写进日志。
    ModelDetected {
        model: zte_g7615_panel::model::Model,
        removed: Vec<String>,
        source: String,
        /// 按机型调整后的按钮分组。UI 用它替换掉启动时的清单 ——
        /// 不带这个字段的话，识别结果就只影响日志，按钮还是老样子。
        groups: Vec<(String, Vec<zte_g7615_panel::presets::Action>)>,
    },
    /// 清空回读校验卡片。
    ///
    /// 为什么不在界面线程直接 clear：
    /// 「清空」和「worker 正要发一条新结论」是两条并发路径。
    /// 点「清空」的同时 worker 完成了一次校验，消息会排在清空之后 ——
    /// 用户点了清空，卡片上却立刻又冒出一条**执行前**的结论。
    /// 让 worker 自己清，两条消息在同一个通道里严格有序，就不会串。
    VerifyCleared,
    /// worker 线程已退出（panic 或通道断开）。此后所有命令都不会被执行，
    /// 界面必须禁用相关控件并提示重启，否则用户点「连接」会永远转圈。
    ///
    /// 带上 panic 的原因：以前界面只知道「工作线程异常退出」，
    /// 用户能提供的唯一线索是「它就那么停了」。把 panic 信息带出来，
    /// 至少日志里能看出是哪一步崩的。
    WorkerDead(String),
    /// 刷机级闸门的备份结论。
    ///
    /// `path == Some` 才代表**原值真的落盘了**，闸门才允许往下走。
    /// 失败时 `error` 带原因，界面据此保持第 ②③ 步禁用。
    CriticalBackupDone {
        /// 对应 `ToWorker::CriticalBackup` 的序号；对不上就是过期结论。
        token: u64,
        path: Option<String>,
        error: Option<String>,
        /// 是否读到了当前区域码（读不到就别让用户手填猜）
        region_detected: bool,
        /// 备份里实际覆盖了哪几张表。界面要按这个把「已备份」说清楚，
        /// 而不是只说「读了 8 张里的 1 张」也叫已备份。
        tables: Vec<String>,
    },
}

// ---------------- 命令预设 ----------------
// Action 与预设加载已移到 lib 的 `presets` 模块：
// 这样 tests/ 下的端到端测试能直接跑**真实**的预设命令列表，
// 不必另写一份容易和产品脱节的副本。
// 清单本身来自 `命令预设.txt`（数据驱动，见 presets.rs / config.rs）。
use zte_g7615_panel::config;
use zte_g7615_panel::presets::{self, set_time_cmd_from_beijing, Action, Loaded};

/// 把预设里的占位命令替换成点击时刻的真实命令。
///
/// 目前只有一个占位符：
/// - `__SET_TIME_NOW__` → `date -s "<当前北京时间换算成 UTC>"`
///
/// 之所以用占位符而不是在 `presets()` 里直接写死时间，
/// 是因为预设是启动时构建一次的常量列表，而校时必须用**点击那一刻**的时间。
///
/// **任何未经替换的占位符都不允许下发到设备**——
/// 发过去只会得到一句 `sh: __SET_TIME_NOW__: not found`。
fn expand_placeholders(cmds: &[String]) -> Vec<String> {
    cmds.iter()
        .map(|c| match placeholder_of(c) {
            Some(tok) => expand_one(tok),
            None => c.clone(),
        })
        .collect()
}

/// 预设里**允许出现**的占位命令全集。
///
/// 漏登记的后果不是「显示个怪字符串」：占位命令会原样发到设备，
/// 设备回一句 `sh: __XXX__: not found`，用户看着界面上的「完成」，
/// 以为操作生效了。
const PLACEHOLDERS: &[&str] = &["__SET_TIME_NOW__"];

/// 一个 token 是否长得像占位符：前后双下划线、长度大于 4。
fn looks_like_placeholder(tok: &str) -> bool {
    tok.len() > 4 && tok.starts_with("__") && tok.ends_with("__")
}

/// 如果整条命令**恰好**是一条已登记的占位命令，返回它。
///
/// 「恰好」是关键：占位符代表的是**整条命令**（比如 `__SET_TIME_NOW__`
/// 会被换成 `date -s "..."`）。写成 `date -s __SET_TIME_NOW__` 这种
/// 复合形态时，替换出来的东西是另一条完整命令，塞进参数位置毫无意义，
/// 而旧实现只按整串相等判断替换、校验却按 token 判断是否登记，
/// 两边口径不一致 —— 于是这种写法能通过校验、又不被替换，
/// 字面量 `__SET_TIME_NOW__` 就这么发到了设备上。
fn placeholder_of(cmd: &str) -> Option<&'static str> {
    let t = cmd.trim();
    PLACEHOLDERS.iter().copied().find(|p| *p == t)
}

/// 把一条已登记的占位命令换成真实命令。
fn expand_one(tok: &str) -> String {
    match tok {
        "__SET_TIME_NOW__" => set_time_now_utc(),
        // PLACEHOLDERS 里的每一项都必须在这里有对应分支，
        // 漏了就等于把占位符原样发给设备。
        other => other.to_string(),
    }
}

/// 检查一份命令里是否残留**会被原样下发**的占位符，返回第一处出问题的命令。
///
/// 两种残留都算：
/// 1. 没登记的占位 token（预设写错一个字，或新增占位符忘了接处理逻辑）；
/// 2. 登记了、但没独占一条命令（`date -s __SET_TIME_NOW__`），
///    替换逻辑不会处理它，发过去就是字面量。
///
/// 判定口径必须和 `expand_placeholders` 完全一致，
/// 否则就会出现「校验放行、替换不管」的缝隙。
fn unexpanded_placeholder(cmds: &[String]) -> Option<&String> {
    cmds.iter().find(|c| {
        let has_token = c.split_whitespace().any(looks_like_placeholder);
        if !has_token {
            return false;
        }
        // 整条命令就是一条已登记的占位命令 → 会被正常替换
        placeholder_of(c).is_none()
    })
}

/// 取当前本地（北京时间）时刻，减 8 小时换算成 UTC 后生成校时命令。
///
/// 光猫时钟是 UTC，国内直连 ntp 不通、重启即丢时间，
/// 所以校时必须按 UTC 下发，否则会差 8 小时。
fn set_time_now_utc() -> String {
    // 用系统本地时间当作北京时间：本机时区就是 Asia/Shanghai。
    // 不引 chrono 依赖——只为了取 6 个数字不值得加一个时间库，
    // 这里的日期换算逻辑在 presets 里已有完整单测覆盖。
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (y, mo, d, h, mi, s) = unix_to_local_beijing(now);
    set_time_cmd_from_beijing(y, mo, d, h, mi, s)
}

/// 把 Unix 秒换算成北京时间（UTC+8）的「年月日时分秒」。
///
/// 只做 UTC+8 的固定偏移换算，不处理夏令时——中国大陆不实行夏令时。
fn unix_to_local_beijing(secs: u64) -> (i32, u32, u32, u32, u32, u32) {
    let local = secs + 8 * 3600;
    let days = (local / 86400) as i64;
    let rem = local % 86400;
    let (h, mi, s) = ((rem / 3600) as u32, ((rem % 3600) / 60) as u32, (rem % 60) as u32);

    // 从 1970-01-01 起推算日期（civil_from_days 算法，Howard Hinnant）
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };

    (y as i32, m, d, h, mi, s)
}

// ---------------- 回读校验的 UI 结论 ----------------

/// 回读校验在界面上的三档结论。
///
/// 为什么不直接用 `bool`：**「设备遮蔽了该字段」和「写入失败」
/// 是完全不同的两件事**。实测 F650 的 `WLANPSK.KeyPassphrase`
/// 写入前后回读都是 `******`，若并成一档，用户会看到一个永远红色的 ❌，
/// 而事实上写入成功了 —— 那是假警报。
///
/// 同理也不能并成「成功」：那是假保证。只能如实说「不知道」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerifyResult {
    /// 回读确认已生效
    Confirmed,
    /// 未生效
    Failed,
    /// 设备遮蔽了该字段，无法确认（已知：WiFi 密码）
    Masked,
}

// ---------------- Worker 线程 ----------------

/// 写后回读校验：回读目标表，给出三档结论与可直接展示的说明。
///
/// 抽成函数是因为 `Run` 和 `RunRaw` 两条路径原本各抄了一份**完全相同**的
/// 判定与文案逻辑。两份代码里的「遮蔽」提示必须字字一致，
/// 抄错一处，用户看到的就是两套互相矛盾的解释。
fn verify_after_write(t: &mut Telnet, cmd: &str, readback: &str) -> (VerifyResult, String) {
    match t.run(readback) {
        Ok(out) => {
            let table = dbcli::parse_table(&out);
            match dbcli::verdict(cmd, &table) {
                dbcli::Verdict::Confirmed => (
                    VerifyResult::Confirmed,
                    format!("回读 {} 确认已生效", table.name),
                ),
                dbcli::Verdict::Failed(why) => (VerifyResult::Failed, why),
                dbcli::Verdict::Masked => (
                    VerifyResult::Masked,
                    format!(
                        "{} 是敏感字段，设备回读时遮蔽为 ******，无法确认是否写入成功",
                        dbcli::field_of(cmd).unwrap_or_else(|| "该字段".into())
                    ),
                ),
            }
        }
        // 回读本身失败：不能因此判定写入失败，但也绝不能报「已生效」。
        Err(e) => (
            VerifyResult::Masked,
            format!("回读失败，无法确认是否生效: {e}"),
        ),
    }
}

/// 写操作执行前，把涉及的表原值抓下来存盘。
///
/// 光猫的 `sendcmd 1 DB set` 立即生效、没有撤销键，
/// 改错了在界面上看不出异常，而设备行为可能已经变了。
/// 备份让「改坏了」有据可查、有可能回退。
///
/// 备份失败**不阻断**操作：抓不到原值固然可惜，但因为备份失败
/// 就不让用户操作反而更糟（有些设备会拒绝读某些表）。
/// 失败信息会写进日志，用户知道这次没有备份。
fn backup_tables_before_write(
    t: &mut Telnet,
    title: &str,
    cmds: &[String],
    log: &dyn Fn(String),
) {
    let tables = zte_g7615_panel::backup::tables_to_backup(cmds);
    if tables.is_empty() {
        return; // 没有写表命令，没什么可备份
    }

    let mut captured: Vec<(String, String)> = Vec::new();
    for name in &tables {
        match t.run(&format!("sendcmd 1 DB p {name}")) {
            Ok(out) => captured.push((name.clone(), out)),
            Err(e) => log(format!("[备份] 读取表 {} 失败: {}（该表未备份）", name, e)),
        }
    }
    if captured.is_empty() {
        log("[备份] 未能读取任何原值，本次操作没有备份".into());
        return;
    }

    let now = current_beijing_time();
    let entry = zte_g7615_panel::backup::BackupEntry {
        timestamp: zte_g7615_panel::backup::format_timestamp(
            now.0, now.1, now.2, now.3, now.4, now.5,
        ),
        action: title.to_string(),
        tables: captured.clone(),
    };

    match zte_g7615_panel::backup::backup_dir() {
        Some(dir) => match zte_g7615_panel::backup::save(&dir, &entry) {
            Ok(path) => log(format!(
                "[备份] 已存档 {} 张表的原值 → {}",
                captured.len(),
                path.display()
            )),
            Err(e) => log(format!("[备份] 写盘失败: {}（原值仍显示在下方）", e)),
        },
        None => log("[备份] 无法确定备份目录（取不到 exe 路径）".into()),
    }

    // 无论写盘成功与否，原值都显示在日志里 ——
    // 这是用户唯一能立刻看到「改之前是什么」的地方。
    for (name, text) in &captured {
        log(format!("--- {} 原值 ---\n{}", name, text));
    }
}

/// 准备本次运行的日志文件，返回路径。
///
/// 放在 exe 同目录的 `运行日志/` 下，文件名带启动时间戳，
/// 每次运行一个新文件（便于「今天那次改坏了」的回溯），
/// 但只保留最近 N 个，避免无限堆积。
fn prepare_log_file() -> Option<std::path::PathBuf> {
    /// 保留的日志文件个数上限
    const KEEP_LOGS: usize = 20;

    let dir = zte_g7615_panel::paths::log_dir()?;
    if std::fs::create_dir_all(&dir).is_err() {
        return None;
    }

    let t = current_beijing_time();
    let stamp = format!(
        "{:04}{:02}{:02}_{:02}{:02}{:02}",
        t.0, t.1, t.2, t.3, t.4, t.5
    );
    let path = dir.join(format!("运行日志_{stamp}.txt"));

    // 清理旧日志：按文件名排序即按时间排序（时间戳定长）
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut files: Vec<std::path::PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().map(|e| e == "txt").unwrap_or(false))
            .collect();
        files.sort();
        // 超出上限时删最早的；留出本次要新建的一个位置
        while files.len() >= KEEP_LOGS {
            if let Some(oldest) = files.first().cloned() {
                let _ = std::fs::remove_file(&oldest);
                files.remove(0);
            } else {
                break;
            }
        }
    }

    // 先创建空文件，确保 append 时目录一定存在且可写
    if std::fs::write(&path, "").is_err() {
        return None;
    }
    Some(path)
}

/// 取当前北京时间，供备份时间戳使用。
fn current_beijing_time() -> (i32, u32, u32, u32, u32, u32) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    unix_to_local_beijing(secs)
}

fn worker_loop(rx: Receiver<ToWorker>, tx: Sender<ToUi>) {
    let mut conn: Option<Telnet> = None;
    let log = |s: String| {
        let _ = tx.send(ToUi::Log(s));
    };

    while let Ok(msg) = rx.recv() {
        match msg {
            ToWorker::Connect { host, port, user, pwd } => {
                // 先丢掉可能已经失效的旧连接，避免失败后界面仍显示「已连接」
                conn = None;
                let _ = tx.send(ToUi::Connecting(true));
                log(format!("\n正在连接 {}:{} ...", host, port));
                match Telnet::connect(&host, &port, &user, &pwd) {
                    Ok(mut t) => {
                        let _ = tx.send(ToUi::Status(true));
                        let _ = tx.send(ToUi::Connecting(false));
                        log(format!("[已连接到 {}，账号 {}]", host, user));

                        // ---- 机型识别 ----
                        // 连上后先探测是哪款光猫，据此选择命令清单。
                        // 不同机型的表名/字段名/可用命令差异很大
                        // （F650 上 `upgradetest getver` 直接 function not found），
                        // 不识别机型就会有一堆按钮点了报错。
                        log("正在识别机型...".into());
                        let mut outputs: Vec<(String, String)> = Vec::new();
                        for cmd in zte_g7615_panel::model::probe_commands() {
                            match t.run(cmd) {
                                Ok(out) => outputs.push(((*cmd).to_string(), out)),
                                Err(e) => {
                                    log(format!("[识别] 探测命令 {cmd} 失败: {e}"));
                                }
                            }
                        }
                        let evidence =
                            zte_g7615_panel::model::extract_evidence(&outputs);
                        let model = evidence.detect();
                        log(format!(
                            "[机型] {}（SoC: {}，型号: {}）",
                            model.display_name(),
                            evidence.hardware.as_deref().unwrap_or("未知"),
                            evidence.model_name.as_deref().unwrap_or("未知"),
                        ));

                        // 按机型加载对应清单
                        let (groups, source, removed) = match zte_g7615_panel::presets::load_for_model(model) {
                            zte_g7615_panel::presets::Loaded::Builtin(g) => {
                                let (g, rm) = zte_g7615_panel::presets::filter_for_model(model, g);
                                (g, "内置清单".to_string(), rm)
                            }
                            zte_g7615_panel::presets::Loaded::FromFile(g, src) => {
                                let (g, rm) = zte_g7615_panel::presets::filter_for_model(model, g);
                                (g, src, rm)
                            }
                            zte_g7615_panel::presets::Loaded::Failed(msg, src) => {
                                log(format!("[预设文件解析失败] {msg}（{src}），已回退到内置清单"));
                                let (g, rm) = zte_g7615_panel::presets::filter_for_model(
                                    model,
                                    zte_g7615_panel::presets::builtin(),
                                );
                                (g, format!("{src}（解析失败）"), rm)
                            }
                        };
                        if !removed.is_empty() {
                            log(format!(
                                "[机型] 已隐藏 {} 条本机型不支持的命令：{}",
                                removed.len(),
                                removed.join("、")
                            ));
                        }
                        // 连上之后顺手核对一遍清单里的占位命令。
                        //
                        // 解析器只能判断「有 group、有 cmd」，看不出 `__SETTIME_NOW__`
                        // 这种写错一个字的占位符 —— 那要等用户点了按钮、
                        // 设备回一句 `sh: ...: not found` 才会暴露。这里提前提示，
                        // 不阻断任何操作（预设仍照常可用）。
                        for (gname, gactions) in &groups {
                            for act in gactions {
                                if let Some(c) = unexpanded_placeholder(&act.cmds) {
                                    log(format!(
                                        "[预设] {}/{} 的命令里有未登记的占位符，点它会被拒绝下发：{}（可选：{}）",
                                        gname,
                                        act.name,
                                        c,
                                        PLACEHOLDERS.join(" ")
                                    ));
                                }
                            }
                        }
                        let _ = tx.send(ToUi::ModelDetected {
                            model,
                            removed,
                            source,
                            groups,
                        });

                        conn = Some(t);
                        log("[已就绪]\n".into());
                    }
                    Err(e) => {
                        let _ = tx.send(ToUi::Status(false));
                        let _ = tx.send(ToUi::Connecting(false));
                        log(format!("[连接失败] {}\n", e));
                    }
                }
            }
            ToWorker::Disconnect => {
                conn = None;
                let _ = tx.send(ToUi::Status(false));
                log("[已断开连接]\n".into());
            }
            ToWorker::Run { title, cmds } => {
                let _ = tx.send(ToUi::Busy(true));
                // 新一批写操作开始 → 先清掉上一批的回读结论（见 VerifyCleared 的说明）
                let _ = tx.send(ToUi::VerifyCleared);
                log(format!("\n===== {} =====", title));

                // ---- 下发前最后一道闸：残留占位命令一律不下发 ----
                //
                // 这些命令来自预设文件（用户可编辑）。旧代码只在 `expand_placeholders`
                // 里比对 `__SET_TIME_NOW__` 一个字符串，写错一个字就会原样发到设备。
                // 直接 `continue` 而不是 `break`：for 循环在 worker_loop 里，
                // break 会跳出整个消息循环、把 worker 弄死。
                if let Some(c) = unexpanded_placeholder(&cmds) {
                    log(format!("[错误] 命令里有未展开的占位符，整批已拒绝下发：{c}"));
                    log("----- 已取消 -----\n".into());
                    let _ = tx.send(ToUi::Busy(false));
                    continue;
                }
                // None = 正常跑完；Some = 中断/异常的说明，用来替代误导性的「完成」
                let mut note: Option<String> = None;
                // 本次操作里有多少条写命令通过了回读校验 / 共校验了多少条
                let mut verified_ok = 0usize;
                let mut verified_failed = 0usize;
                let mut verified_total = 0usize;

                // ---- 写前备份 ----
                // 写操作立即生效且没有撤销键，改错了在界面上看不出来。
                // 所以先把目标表的原值抓下来存盘。
                if let Some(t) = conn.as_mut() {
                    backup_tables_before_write(t, &title, &cmds, &log);
                }

                for c in &cmds {
                    log(format!("> {}", c));
                    match conn.as_mut() {
                        Some(t) => match t.run(c) {
                            Ok(o) => {
                                if o != "(无输出)" {
                                    log(o);
                                }
                                // ---- 写后回读校验 ----
                                // sendcmd 1 DB set 在字段名/行号写错时不报错、不输出，
                                // 界面显示「完成」而配置根本没写进去。
                                // 这里立刻回读该表并比对目标字段。
                                if let Some(rb) = dbcli::readback_cmd_for(c) {
                                    verified_total += 1;
                                    let (result, detail) =
                                        verify_after_write(t, c, &rb);
                                    if result == VerifyResult::Confirmed {
                                        verified_ok += 1;
                                    } else if result == VerifyResult::Failed {
                                        verified_failed += 1;
                                    }
                                    let _ = tx.send(ToUi::Verify {
                                        cmd: c.clone(),
                                        result,
                                        detail,
                                    });
                                }
                            }
                            Err(e) => {
                                let is_reboot = c.trim() == "reboot";
                                // reboot 必然断链；其余命令只有断链类错误才丢弃连接，
                                // 超时（命令可能还在跑）时连接本身仍然可用
                                let lost = is_reboot || e.kind == ErrKind::Disconnected;
                                if is_reboot {
                                    log("[连接已断开，光猫正在重启...]".into());
                                    // 重启后必然断链，这是预期结果，不算执行失败
                                    note = Some("已执行：光猫正在重启，请稍后重新连接".into());
                                } else if e.kind == ErrKind::Timeout {
                                    log(format!("[超时] {}", e));
                                    note = Some(format!("中断：{} 未完成（超时）", c));
                                } else {
                                    log(format!("[错误] {}", e));
                                    note = Some(format!("中断：{} 未完成", c));
                                }
                                if lost {
                                    conn = None;
                                    let _ = tx.send(ToUi::Status(false));
                                }
                                break;
                            }
                        },
                        None => {
                            log("[错误] 尚未连接".into());
                            note = Some(format!("中断：{} 未执行（尚未连接）", c));
                            break;
                        }
                    }
                }
                // 校验结论汇总：有写命令却没通过校验时，必须显式警告，
                // 否则「----- 完成 -----」会继续骗人。
                //
                // 遮蔽（Masked）单独计数：它是「无法确认」而非「失败」，
                // 混进失败数会制造假警报。
                if verified_total > 0 {
                    let masked = verified_total - verified_ok - verified_failed;
                    log(format!(
                        "----- 回读校验：{}/{} 条写命令已确认生效{} -----",
                        verified_ok,
                        verified_total,
                        if masked > 0 {
                            format!("，{} 条因设备遮蔽无法确认", masked)
                        } else {
                            String::new()
                        }
                    ));
                    if verified_failed > 0 && note.is_none() {
                        note = Some(format!(
                            "注意：{} 条写命令未通过回读校验，配置可能没有真正写入",
                            verified_failed
                        ));
                    }
                }
                match note {
                    Some(n) => log(format!("----- {} -----\n", n)),
                    None => log("----- 完成 -----\n".into()),
                }
                let _ = tx.send(ToUi::Busy(false));
            }
            ToWorker::CriticalBackup {
                token,
                title,
                tables,
                probe_cmds,
                planned_cmds,
            } => {
                let _ = tx.send(ToUi::Busy(true));
                let _ = tx.send(ToUi::VerifyCleared);
                log(format!("\n===== {} =====", title));

                let mut captured: Vec<(String, String)> = Vec::new();
                let mut failures: Vec<String> = Vec::new();

                // ---- 1) 读原值 ----
                for name in &tables {
                    let cmd = format!("sendcmd 1 DB p {name}");
                    log(format!("> {cmd}"));
                    match conn.as_mut() {
                        Some(t) => match t.run(&cmd) {
                            Ok(out) => {
                                if out != "(无输出)" {
                                    log(out.clone());
                                }
                                captured.push((name.clone(), out));
                            }
                            Err(e) => {
                                log(format!("[备份] 读取 {name} 失败: {e}"));
                                failures.push(format!("{name}: {e}"));
                            }
                        },
                        None => {
                            failures.push("尚未连接".into());
                            break;
                        }
                    }
                }

                // ---- 2) 顺带跑只读探测（区域码 / 串码）----
                // 顺便判断有没有读到区域码：读不到就得让用户手填并核对。
                let mut region_probe_seen = false;
                if conn.is_some() {
                    for cmd in &probe_cmds {
                        log(format!("> {cmd}"));
                        if let Some(t) = conn.as_mut() {
                            match t.run(cmd) {
                                Ok(out) => {
                                    if out != "(无输出)" {
                                        log(out.clone());
                                    }
                                    if cmd.contains("flag_type") {
                                        region_probe_seen =
                                            crate::CriticalForm::parse_region(&out).is_some();
                                    }
                                }
                                Err(e) => log(format!("[探测] {cmd} 失败: {e}")),
                            }
                        }
                    }
                }

                // ---- 3) 落盘 ----
                // 一张表都没读到时**不许**报成功：那会放行用户去改区域，
                // 而他手上那份「备份」里没有任何设备原值。
                let mut path: Option<String> = None;
                let mut error: Option<String> = None;
                if captured.is_empty() {
                    error = Some(if failures.is_empty() {
                        "未能读取任何表的原值".to_string()
                    } else {
                        format!("未能读取任何表的原值：{}", failures.join("；"))
                    });
                } else {
                    let now = current_beijing_time();
                    let ts = zte_g7615_panel::backup::format_timestamp(
                        now.0, now.1, now.2, now.3, now.4, now.5,
                    );
                    let mut entries = captured.clone();
                    // 把「即将执行什么」也附进去，方便事后对照
                    entries.push((
                        "即将执行的命令".to_string(),
                        planned_cmds.join("\n"),
                    ));
                    let entry = zte_g7615_panel::backup::BackupEntry {
                        timestamp: ts,
                        action: title.clone(),
                        tables: entries,
                    };
                    match zte_g7615_panel::backup::backup_dir() {
                        Some(d) => match zte_g7615_panel::backup::save(&d, &entry) {
                            Ok(p) => {
                                log(format!(
                                    "[备份] 已存档 {} 张表的原值 → {}",
                                    captured.len(),
                                    p.display()
                                ));
                                path = Some(p.display().to_string());
                            }
                            Err(e) => error = Some(format!("备份写盘失败: {e}")),
                        },
                        None => error = Some("取不到备份目录".into()),
                    }
                }

                // 读到了原值但有个别表失败：照常放行，但把缺口写进日志
                if !failures.is_empty() && error.is_none() {
                    log(format!(
                        "[备份] 注意：{} 张表未备份成功：{}",
                        failures.len(),
                        failures.join("；")
                    ));
                }

                match &path {
                    Some(_) => log("----- 备份完成 -----\n".into()),
                    None => log("----- 备份失败，闸门保持锁定 -----\n".into()),
                }
                // 探测命令的输出已经作为 Log 消息发出去了，
                // 这里只回报「有没有读到区域码」，让界面决定要不要提示手填。
                let region_detected = region_probe_seen;
                let done_tables: Vec<String> =
                    captured.iter().map(|(n, _)| n.clone()).collect();
                let _ = tx.send(ToUi::CriticalBackupDone {
                    token,
                    path,
                    error,
                    region_detected,
                    tables: done_tables,
                });
                let _ = tx.send(ToUi::Busy(false));
            }
            ToWorker::ClearVerifications => {
                let _ = tx.send(ToUi::VerifyCleared);
            }
            ToWorker::RunRaw { cmd } => {
                // 自定义命令：行为和分组操作一致，同样带回读校验
                let _ = tx.send(ToUi::Busy(true));
                // 新一批写操作开始 → 先清掉上一批的回读结论。
                // 卡片只留最近 50 条，不清的话用户看到的是两批混在一起，
                // 分不清哪条 ❌ 是刚才那次留下的。
                let _ = tx.send(ToUi::VerifyCleared);
                log(format!("\n===== 自定义: {} =====", cmd));
                // 占位命令是预设文件里的记号，用户手打的命令里出现它只会是笔误；
                // 原样发过去等于让设备执行一条不存在的命令。
                if let Some(c) = unexpanded_placeholder(std::slice::from_ref(&cmd)) {
                    log(format!("[错误] 命令里有未展开的占位符，已拒绝下发：{c}"));
                    log("----- 已取消 -----\n".into());
                    let _ = tx.send(ToUi::Busy(false));
                    continue;
                }
                if let Some(t) = conn.as_mut() {
                    // 自定义命令也可能是写操作，同样先备份
                    backup_tables_before_write(t, &format!("自定义: {cmd}"), std::slice::from_ref(&cmd), &log);
                }
                match conn.as_mut() {
                    Some(t) => match t.run(&cmd) {
                        Ok(o) => {
                            if o != "(无输出)" {
                                log(o);
                            }
                            if let Some(rb) = dbcli::readback_cmd_for(&cmd) {
                                let (result, detail) = verify_after_write(t, &cmd, &rb);
                                let _ = tx.send(ToUi::Verify {
                                    cmd: cmd.clone(),
                                    result,
                                    detail,
                                });
                            }
                        }
                        Err(e) => {
                            let is_reboot = cmd.trim() == "reboot";
                            if is_reboot {
                                log("[连接已断开，光猫正在重启...]".into());
                                log("----- 已执行：光猫正在重启，请稍后重新连接 -----\n".into());
                            } else {
                                log(format!("[错误] {}", e));
                            }
                            if is_reboot || e.kind == ErrKind::Disconnected {
                                conn = None;
                                let _ = tx.send(ToUi::Status(false));
                            }
                        }
                    },
                    None => log("[错误] 尚未连接".into()),
                }
                let _ = tx.send(ToUi::Busy(false));
            }
        }
    }
}

/// 把 `catch_unwind` 拿到的 panic 载荷转成一行能读的文字。
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "原因未知".to_string()
    }
}

/// worker 线程的兜底：一旦 panic，界面会永远卡在「连接中」且无法恢复，
/// 所以把状态全部复位并提示用户。
fn spawn_worker(rx_w: Receiver<ToWorker>, tx_u: Sender<ToUi>) {
    let tx_panic = tx_u.clone();
    std::thread::spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            worker_loop(rx_w, tx_u)
        }));
        if let Err(payload) = r {
            let _ = tx_panic.send(ToUi::Status(false));
            let _ = tx_panic.send(ToUi::Connecting(false));
            let _ = tx_panic.send(ToUi::Busy(false));
            // 必须显式告知「worker 已死」：否则 UI 仍允许点「连接」，
            // 而消息会进到一个没有接收者的通道，界面永远停在「连接中...」
            let why = panic_text(payload.as_ref());
            let _ = tx_panic.send(ToUi::WorkerDead(why.clone()));
            let _ = tx_panic.send(ToUi::Log(format!(
                "\n[内部错误] 工作线程异常退出（{why}），命令功能已停用，请重启程序\n"
            )));
        }
    });
}

// ---------------- 主应用 ----------------
struct App {
    tx: Sender<ToWorker>,
    rx: Receiver<ToUi>,
    groups: Vec<(String, Vec<Action>)>,
    host: String,
    port: String,
    user: String,
    pwd: String,
    custom: String,
    show_pwd: bool,
    log: String,
    connected: bool,
    connecting: bool,
    busy: bool,
    /// 待确认的危险操作：(显示名, 命令, 风险说明)
    pending: Option<(String, Vec<String>, Option<String>)>,
    /// worker 线程是否已退出。为 true 时禁用所有会发命令/连接的控件。
    worker_dead: bool,
    /// worker 退出的原因（panic 载荷）。空串表示不是 panic 导致的。
    ///
    /// 以前只记「死了」不记「为什么」，用户能提供的线索就只剩「它停了」，
    /// 排查时等于从零开始。
    worker_dead_reason: String,
    /// 回读校验的结果记录，最近的排在最前面。
    /// 只留最近 N 条：用户关心的是「刚才那批写进去了没有」。
    verifications: Vec<(String, VerifyResult, String)>,
    /// 「读整表」要查的表名
    table_query: String,
    /// 当前生效的预设清单来自哪里，以及是否加载失败。
    /// 必须显示出来——否则「我改了文件怎么没生效」会变成查不出原因的坑。
    preset_source: String,
    preset_error: Option<String>,
    /// 正在等待参数输入的操作。
    ///
    /// `Some` 时界面弹输入表单，不把命令发出去 ——
    /// 否则 `$参数名` 会被原样发到设备，用户以为改成功了。
    param_form: Option<ParamForm>,
    /// 本次会话的日志文件路径。每条日志同时追加进去，
    /// 避免内存上限（256KB）把最早的记录冲掉。
    log_file: Option<std::path::PathBuf>,
    /// 当前日志文件已写入的字节数，用来判断要不要轮转。
    ///
    /// 不每帧去 metadata() 问一次文件系统：那是同步 IO，
    /// 而这条路径每来一行日志就要走一遍。
    log_file_bytes: u64,
    /// 识别出的机型。`None` 表示还没连过设备。
    detected_model: Option<zte_g7615_panel::model::Model>,
    /// 因机型不支持而被隐藏的命令。显示在界面上，
    /// 让用户知道「少的是因为机型不支持」而不是程序坏了。
    hidden_commands: Vec<String>,

    // ---- 以下纯界面状态，不影响任何设备通信 ----
    /// 侧栏当前宽度。由拖拽改变，用于判断要不要进「图标模式」。
    sidebar_width: f32,
    /// 侧栏搜索关键词，只过滤显示，不改预设本身。
    search: String,
    /// 搜索框是否展开（Ctrl+K 会展开它并抢焦点）。
    show_search: bool,
    /// 日志是否自动滚到底。用户可以关掉它去翻历史输出。
    auto_scroll: bool,
    /// 启动时是否展开全部分组。只由 `--open-groups` 打开（验收截图用），
    /// 正常运行保持折叠 —— 55 个操作全铺开会把侧栏撑成一长条。
    open_all_groups: bool,
    /// 当前外观选择（浅色 / 深色 / 跟随系统）。默认跟随系统。
    theme_pref: egui::ThemePreference,
    /// 刷机级备份的序号发号器。
    ///
    /// 单调递增，跨闸门的「打开 → 取消 → 再打开」也不会重复，
    /// 过期的备份结论据此被识别并丢弃。
    critical_backup_seq: u64,
    /// 刷机级操作（改区域）的确认状态。`Some` 时界面弹专用闸门。
    critical: Option<CriticalForm>,
}

/// 参数输入表单的状态。
struct ParamForm {
    /// 操作显示名
    title: String,
    /// 命令模板（含 `$参数名`）
    cmds: Vec<String>,
    /// 风险说明
    note: Option<String>,
    /// 参数定义 + 当前输入值
    fields: Vec<(config::ParamDef, String)>,
    /// 校验失败的原因，显示在表单里
    error: Option<String>,
    /// 是否已经点了「执行」；用于把危险操作的二次确认串起来：
    /// 参数合法 → 再走危险确认条 → 才真正下发
    danger: bool,
}


/// 刷机级操作（改区域）的确认状态机。
///
/// 为什么不是「一个 bool + 一个输入框」：改区域是**不可逆的设备重置**，
/// 必须强制用户依次完成「备份 → 逐条确认 → 双输入」，任何一步都不能跳过。
/// 把状态收进结构体而不是散在 App 里，是因为这几个字段的**耦合关系**
/// 本身就是规则（没备份就不许勾选、没勾满就不许执行），
/// 集中在一起才能用一条 `validate()` 完整表达。
struct CriticalForm {
    title: String,
    /// 原始命令模板（含 `$目标区域码`）
    cmds: Vec<String>,
    note: Option<String>,
    /// 步骤①要下发的只读探测命令
    probe_cmds: Vec<String>,
    /// 备份写盘后的路径（None = 还没备份）
    backup_path: Option<String>,
    backup_error: Option<String>,
    /// 从 flag_type 读到的当前区域码
    current_detected: Option<String>,
    /// 三条确认
    ack_reboot: bool,
    ack_config: bool,
    ack_register: bool,
    /// 备份时到底读到了哪几张关键表。
    ///
    /// 闸门原来的文案是「已备份」，可只要读到任意一张表就会显示这两个字 ——
    /// 断网、只有一张表读得动的时候，用户看到的同样是「已备份」，
    /// 而那份备份里根本没有账号、WAN、Telnet。既然这步的存在意义就是
    /// 「改坏了还能查回原值」，就必须把**覆盖面**摆出来，而不是一句结论。
    backup_tables: Vec<String>,
    /// 用户输入：当前码 / 目标码 / 目标码重复
    current_input: String,
    target_input: String,
    target_confirm: String,
    error: Option<String>,
    /// 本轮备份的序号。
    ///
    /// 每次点「备份并检查」时从 `App` 领一个新号，worker 原样回传；
    /// 回传的号和这里不一致就说明是上一轮（已被取消）的结论，直接丢弃。
    /// 没有它的话，取消后重开闸门会立刻显示「已备份」，
    /// 而这次打开其实一次备份都没跑过。
    backup_token: u64,
    /// 备份/读取正在进行中。
    ///
    /// 这个字段以前**从来没被置为 true**（声明了、初始化了、读了，
    /// 就是没有一处写它），于是「备份并检查」按钮在 worker 已经跑起来
    /// 之后仍然可点：连点两下会压两条 CriticalBackup 进通道，
    /// 第二份备份覆盖第一份，而两次都在往日志里刷。
    /// 现在由 CriticalBackup 的发出与 CriticalBackupDone 的到达成对维护。
    busy: bool,
}

impl CriticalForm {
    fn new(title: String, cmds: Vec<String>, note: Option<String>) -> Self {
        Self {
            title,
            cmds,
            note,
            // 三条只读探测：都是查询，不改设备状态。
            // 顺序按「最可能失败的在最后」排，前面的能先给出有用信息。
            probe_cmds: vec![
                "cat /userconfig/flag_type".to_string(),
                "cat /etc/init.d/regioncode".to_string(),
                "setmac show2".to_string(),
            ],
            backup_path: None,
            backup_error: None,
            backup_tables: Vec::new(),
            current_detected: None,
            ack_reboot: false,
            ack_config: false,
            ack_register: false,
            current_input: String::new(),
            target_input: String::new(),
            target_confirm: String::new(),
            error: None,
            backup_token: 0,
            busy: false,
        }
    }

    /// 从 `cat /userconfig/flag_type` 的输出里抠出当前区域码。
    ///
    /// 典型输出是 `current :  334`。解析失败返回 None ——
    /// 闸门会因此不放行，而不是猜一个。
    fn parse_region(log: &str) -> Option<String> {
        for line in log.lines().rev() {
            let l = line.trim();
            // 只认 `current` 打头、紧跟 `=` 或 `:` 的行。
            // 光凭「这一行含 current 这个词」太松：用户在「自定义命令」里
            // 敲一句 `echo current 305`，日志里就会多出一行带 current 的输出，
            // 而闸门是从整段日志里倒着找的 —— 它会把这个 305 当成设备读数，
            // 于是「当前区域码必须与设备一致」这道校验就被一个假值满足了。
            // 真正的 flag_type 输出形如 `current :  334`，收紧后照样认。
            let after = match l.split_once(|c: char| c == '=' || c == ':') {
                Some((k, v)) if k.trim().eq_ignore_ascii_case("current") => v,
                _ => continue,
            };
            let l = after.trim();
            if let Some(pos) = l.rfind(|c: char| c.is_ascii_digit()) {
                let bytes = l.as_bytes();
                let mut start = pos;
                while start > 0 && bytes[start - 1].is_ascii_digit() {
                    start -= 1;
                }
                let num = &l[start..=pos];
                if num.len() == 3 {
                    return Some(num.to_string());
                }
            }
        }
        None
    }

    /// 备份完成才允许进入第 ② 步。
    fn backup_ok(&self) -> bool {
        self.backup_path.is_some()
    }

    /// 三项都勾了才算确认完毕。
    fn all_acked(&self) -> bool {
        self.ack_reboot && self.ack_config && self.ack_register
    }

    /// 「确认执行」是否可点。
    fn can_execute(&self) -> bool {
        self.backup_ok() && self.all_acked() && !self.busy
    }

    /// 执行前的完整校验。返回第一个不满足的条件。
    ///
    /// 这是**逻辑层**的闸门，与界面上按钮的 enabled 状态相互独立：
    /// 以后加快捷键、加命令行入口时，绕过界面也绕不过这层。
    fn validate(&self) -> Result<(), String> {
        if !self.backup_ok() {
            return Err("还没有备份。请先点「备份并检查」。".into());
        }
        if !self.all_acked() {
            return Err("三条确认必须全部勾选。".into());
        }
        let cur = self.current_input.trim();
        let tgt = self.target_input.trim();
        let cfm = self.target_confirm.trim();

        if cur.is_empty() {
            return Err("请填写当前区域码。".into());
        }
        if cur.len() != 3 || !cur.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("当前区域码应为三位数字，实际是「{cur}」。"));
        }
        // 与设备读到的值比对：不一致说明用户看错了行，或者备份之后区域变了
        if let Some(d) = &self.current_detected {
            if cur != d {
                return Err(format!(
                    "当前区域码与设备读到的不一致：设备是 {d}，你填的是 {cur}。请重新备份核对。"
                ));
            }
        }
        if tgt.is_empty() {
            return Err("请填写目标区域码。".into());
        }
        if tgt.len() != 3 || !tgt.chars().all(|c| c.is_ascii_digit()) {
            return Err(format!("目标区域码应为三位数字，实际是「{tgt}」。"));
        }
        if tgt != cfm {
            return Err(format!(
                "两次输入的目标区域码不一致：「{tgt}」与「{cfm}」。"
            ));
        }
        if tgt == cur {
            return Err("目标区域码与当前区域码相同，不需要执行。".into());
        }
        Ok(())
    }
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_fonts(&cc.egui_ctx);
        // 设计令牌必须在建任何控件之前装好：
        // 之后所有 Frame/Button 的取值都从这里来，晚一步第一帧就是旧配色。
        //
        // 先读外观设置再 install：装早了会用错主题，
        // 首帧闪一下白/黑再跳回来，比一直错还难看。
        let pref: egui::ThemePreference = zte_g7615_panel::appearance::load().into();
        theme::install(&cc.egui_ctx, pref);
        Self::build()
    }

    /// 切换外观：立即生效 + 落盘。
    ///
    /// 落盘失败不打断用户 —— 本次会话已经切过去了，
    /// 下次启动回到默认外观（跟随系统）而已。
    pub fn set_theme_pref(&mut self, ctx: &egui::Context, pref: egui::ThemePreference) {
        self.theme_pref = pref;
        theme::apply_theme(ctx, pref);
        if let Some(p) = zte_g7615_panel::appearance::appearance_path() {
            let choice: zte_g7615_panel::appearance::ThemeChoice = pref.into();
            let _ = zte_g7615_panel::appearance::save_to(&p, choice);
        }
    }

    /// 不依赖 CreationContext 的构造：截图模式自己装字体与主题，
    /// 这里只负责状态。两份构造共用同一段逻辑，避免以后改一处漏一处。
    fn new_headless() -> Self {
        Self::build()
    }

    fn build() -> Self {
        let (tx_w, rx_w) = channel();
        let (tx_u, rx_u) = channel();
        spawn_worker(rx_w, tx_u);

        // 加载预设：优先用 exe 旁边的 `命令预设.txt`。
        // 文件不存在时顺手写一份出去，方便用户编辑。
        let loaded = presets::load();
        let (groups, preset_source, preset_error, file_note) = match loaded {
            Loaded::Builtin(g) => {
                let mut note = String::new();
                if let Some(p) = presets::preset_path() {
                    if presets::ensure_preset_file(&p) {
                        note = format!("已生成默认预设文件：{}（可自行编辑，改完重启生效）\n", p.display());
                    }
                }
                (g, "内置清单".to_string(), None, note)
            }
            Loaded::FromFile(g, src) => (g, src, None, String::new()),
            Loaded::Failed(msg, src) => {
                // 解析失败绝不用坏清单，回落到内置清单，但错误必须显示
                (
                    presets::builtin(),
                    format!("{src}（解析失败，已回退到内置清单）"),
                    Some(msg),
                    String::new(),
                )
            }
        };

        // 首次运行时把旧版遗留在根目录的运行数据搬进 `数据/`。
        // 不做这一步，老用户会以为「设置丢了、日志没了」。
        // 必须排在读取设置之前 —— 搬完再读，老设置才能被读到。
        let migrated = zte_g7615_panel::paths::migrate_legacy_layout();
        let migrate_note = if migrated.is_empty() {
            String::new()
        } else {
            format!("已把旧版数据迁入 数据\\ 目录：{}\n", migrated.join("、"))
        };

        let mut log =
            String::from("提示：填好连接信息后点“连接”，连接成功即可点按钮执行。\n密码不会被保存，每次运行需要重新输入。\n");
        log.push_str(&migrate_note);
        log.push_str(&file_note);
        if let Some(e) = &preset_error {
            log.push_str(&format!(
                "\n[预设文件解析失败] {}\n已回退到内置清单。请修正该文件后重启程序。\n",
                e
            ));
        }


        // 连接参数：记住地址/端口/账号，密码不落盘，每次运行重新输入。
        // 必须排在数据迁移之后 —— 迁移会把旧设置搬进 数据/，先读就读不到。
        let saved = zte_g7615_panel::settings::load();

        // 会话日志文件：`数据/运行日志/`，文件名带启动时间
        let log_file = prepare_log_file();

        let mut app = App {
            tx: tx_w,
            rx: rx_u,
            groups,
            host: saved.host.clone(),
            port: saved.port.clone(),
            user: saved.user.clone(),
            pwd: String::new(),
            custom: String::new(),
            show_pwd: false,
            log,
            connected: false,
            connecting: false,
            busy: false,
            pending: None,
            worker_dead: false,
            worker_dead_reason: String::new(),
            verifications: Vec::new(),
            table_query: String::new(),
            preset_source,
            preset_error,
            param_form: None,
            // 新文件是空文件；万一复用了已存在的同名文件，
            // 也要从它的实际大小起算，否则轮转要等到下一个 8MB。
            log_file_bytes: log_file
                .as_ref()
                .and_then(|p| std::fs::metadata(p).ok())
                .map(|m| m.len())
                .unwrap_or(0),
            log_file,
            detected_model: None,
            hidden_commands: Vec::new(),
            sidebar_width: theme::SIDEBAR_DEFAULT,
            search: String::new(),
            show_search: true,
            auto_scroll: true,
            open_all_groups: false,
            // 外观：读 `数据/外观设置.txt`，缺失或损坏一律回落跟随系统。
            theme_pref: zte_g7615_panel::appearance::load().into(),
            critical_backup_seq: 0,
            critical: None,
        };

        // 启动信息走 append_log 落盘 —— 保证日志文件从第一条起就是完整的。
        // 直接把字符串塞进 self.log 会绕过落盘，结果是文件里缺掉开头。
        let startup = app.log.trim_end().to_string();
        app.log.clear();
        if let Some(p) = app.log_file.clone() {
            app.append_log(&format!("本次运行日志：{}", p.display()));
        }
        for line in startup.lines() {
            app.append_log(line);
        }
        app
    }

/// 灌一份「闸门已经走到最后一步」的假状态，只给 `--critical-demo` 截图用。
    ///
    /// 它不连设备、不写备份、不发任何命令 —— 只是把界面填成
    /// 「备份好了、三项勾了、码也填对了」的样子，
    /// 好让这个正常要操作三步才能看到的界面能被截图验收。
    fn seed_critical_demo(&mut self) {
        let mut form = CriticalForm::new(
            "修改区域码".to_string(),
            vec!["upgradetest sdefconf $目标区域码".to_string()],
            Some(
                "会立即重启；配置重置为该省默认模板（超密回默认、WAN 需重建、Telnet 必掉）；必须重新注册。这是刷机级操作，不是改设置。"
                    .to_string(),
            ),
        );
        form.backup_path = Some("数据\\配置备份\\20261018-224500_区域闸门-修改区域码.txt".to_string());
        form.backup_tables = Self::CRITICAL_BACKUP_TABLES
            .iter()
            .map(|s| s.to_string())
            .collect();
        form.current_detected = Some("305".to_string());
        // 读到了区域码：否则界面会同时显示「已备份（8 张表）」和
        // 「未能从输出里读到区域码」，一张截图自相矛盾。
        self.append_log("> cat /userconfig/flag_type\ncurrent :  305\n");
        form.current_input = "305".to_string();
        form.target_input = "210".to_string();
        form.target_confirm = "210".to_string();
        form.ack_reboot = true;
        form.ack_config = true;
        form.ack_register = true;
        self.critical = Some(form);
    }

    /// 灌一份**纯展示用**的假状态，只给 `--demo` 截图用。
    ///
    /// 它不连设备、不发命令、不写日志文件 —— 只是把界面填成
    /// 「刚跑完一批写操作」的样子，好让回读校验卡片、危险确认这类
    /// 「平时看不到」的界面也能被截图验收。
    fn seed_demo_state(&mut self, with_pending: bool) {
        self.connected = true;
        self.detected_model = Some(zte_g7615_panel::model::Model::F650Series);
        self.log = String::from(
            "本次运行日志：数据\\运行日志\\运行日志_20261018_204500.txt\n\
             [已连接到 192.168.1.1，账号 root]\n\
             正在识别机型...\n\
             [机型] F650 系列（GPON）（SoC: ZX279127，型号: F650）\n\
             [机型] 已隐藏 3 条本机型不支持的命令：upgradetest getver、/proc/capability/boardinfo、setmac 3 40962\n\
             [已就绪]\n\n\
             ===== 设备信息 =====\n\
             > sendcmd 1 DB p DevInfo\n\
             <Tbl name=\"DevInfo\" RowCount=\"1\">\n\
             <DM name=\"ModelName\" val=\"F650\"/>\n\
             <DM name=\"SoftwareVer\" val=\"V2.0.0P1T1\"/>\n\
             </Tbl>\n\
             ----- 完成 -----\n\n\
             ===== WiFi 配置 =====\n\
             > sendcmd 1 DB set WLANCfg 0 ESSIDPrefix Home\n\
             ----- 回读校验：2/3 条写命令已确认生效，1 条因设备遮蔽无法确认 -----\n\n\
             ===== 重启设备 =====\n\
             > reboot\n\
             [连接已断开，光猫正在重启...]\n\
             ----- 已执行：光猫正在重启，请稍后重新连接 -----\n",
        );
        self.verifications = vec![
            (
                "sendcmd 1 DB set WLANCfg 0 ESSIDPrefix Home".to_string(),
                VerifyResult::Confirmed,
                "回读 WLANCfg 确认已生效".to_string(),
            ),
            (
                "sendcmd 1 DB set WLANPSK 0 KeyPassphrase ********".to_string(),
                VerifyResult::Masked,
                "KeyPassphrase 是敏感字段，设备回读时遮蔽为 ******，无法确认是否写入成功"
                    .to_string(),
            ),
            (
                "sendcmd 1 DB set DevAuthInfo 0 Pass admin1234".to_string(),
                VerifyResult::Failed,
                "回读 DevAuthInfo 未找到字段 Pass 的预期值".to_string(),
            ),
        ];
        if with_pending {
            self.pending = Some((
                "固化 Telnet(root/Zte521)".to_string(),
                vec!["sendcmd 1 DB set TelnetCfg 0 Lan_Enable 1".to_string()],
                Some(
                    "会写入公开的弱口令 Zte521 并把关闭时间设为极大值（等于永久开启）。\
                     仅在自己的局域网内使用，用完建议改回强口令。"
                        .to_string(),
                ),
            ));
        }
    }

    /// 提交参数表单：校验 → 展开命令 → 走危险确认或直接执行。
    ///
    /// 校验不过就留在表单里显示原因，**绝不把半成品命令发出去**。
    fn submit_param_form(&mut self) {
        let Some(form) = self.param_form.as_ref() else {
            return;
        };
        let values: Vec<(String, String)> = form
            .fields
            .iter()
            .map(|(d, v)| (d.name.clone(), v.trim().to_string()))
            .collect();

        // 逐项校验：错误信息要能直接指出是哪个参数、该怎么改
        for (def, val) in &form.fields {
            if let Err(why) = def.kind.validate(val) {
                let msg = format!("参数「{}」{}", def.name, why);
                if let Some(f) = self.param_form.as_mut() {
                    f.error = Some(msg);
                }
                return;
            }
        }

        // 展开命令：这一步之后 `$参数名` 全部变成实际值
        let expanded: Vec<String> = form
            .cmds
            .iter()
            .map(|c| config::substitute(c, &values))
            .collect();

        // 兜底①：展开后不该再残留 `$`。真出现了说明模板有问题，
        // 与其把 `$xxx` 发到设备，不如在这里拦住并说明。
        for c in &expanded {
            if let Some(pos) = c.find('$') {
                let rest = &c[pos..];
                // 允许命令里本来就有 `$`（极少见），只拦「看起来像没替换掉的参数名」
                let looks_like_param = rest[1..]
                    .chars()
                    .next()
                    .map(|ch| ch.is_alphanumeric() || ch == '_')
                    .unwrap_or(false);
                if looks_like_param {
                    let msg = format!("命令里仍有未替换的参数：{c}");
                    if let Some(f) = self.param_form.as_mut() {
                        f.error = Some(msg);
                    }
                    return;
                }
            }
        }

        // 兜底②：残留的占位命令同样不许出这道门。
        //
        // 占位符只在 `expand_placeholders` 里替换，而危险操作这条路上
        // 展开是**推迟到二次确认之后**做的 —— 表单里这一份还没展开。
        // 所以这里只看「有没有登记之外的占位符」，不做替换。
        if let Some(c) = unexpanded_placeholder(&expanded) {
            if let Some(f) = self.param_form.as_mut() {
                f.error = Some(format!("命令里有未展开的占位符（预设文件写错？）：{c}"));
            }
            return;
        }

        let title = form.title.clone();
        let note = form.note.clone();
        let danger = form.danger;
        self.param_form = None;

        if danger {
            // 危险操作：参数合法后仍然要走二次确认，
            // 不能因为「填了表单」就跳过一次确认
            self.pending = Some((title, expanded, note));
        } else {
            let _ = self.tx.send(ToWorker::Run {
                title,
                cmds: expand_placeholders(&expanded),
            });
        }
    }

    /// 追加一条日志：先落盘，再进内存缓冲，最后按上限裁剪。
    fn append_log(&mut self, s: &str) {
        self.append_log_to_disk(s);
        self.log.push_str(s);
        self.log.push('\n');
        trim_log(&mut self.log);
    }

    /// 把一行日志追加到会话日志文件。
    ///
    /// 内存里的日志到 256KB 就会丢最早的，而「改坏了回头查」恰恰
    /// 依赖最早的记录。所以每条同时追加到会话日志文件。
    ///
    /// 用 OpenOptions::append 而不是持有句柄：既避免生命周期纠缠，
    /// 也保证程序被强杀时已写入的内容不会丢。
    ///
    /// 文件本身也有上限：内存那份裁了，落盘这份以前没有，
    /// 长会话能把单个文件写到几十 MB，而清理只在下次启动时发生。
    fn append_log_to_disk(&mut self, s: &str) {
        let Some(path) = self.log_file.clone() else {
            return;
        };
        // 超限先轮转，本轮从这个空文件重新写
        if should_rotate_log(self.log_file_bytes) && rotate_log_file(&path).is_ok() {
            self.log_file_bytes = 0;
        }
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            // 写失败不打断界面：日志落盘是辅助功能，
            // 不该因为磁盘满/权限问题让用户无法操作设备。
            if writeln!(f, "{}", s).is_ok() {
                self.log_file_bytes += s.len() as u64 + 1;
            }
        }
    }
}

/// 把日志裁到 `LOG_MAX_BYTES` 以内：从尾部保留，并对齐到行首。
///
/// 单独抽出来是为了能被单测直接驱动 —— 这里是唯一一处会在 UTF-8
/// 字符边界上切字符串的地方，切错了 `drain` 会 panic，
/// 而 `append_log` 本身又要写文件、碰 `log_file`，测试里没法直接调。
fn trim_log(log: &mut String) {
    if log.len() <= LOG_MAX_BYTES {
        return;
    }
    // 从尾部保留 LOG_MAX_BYTES 字节，并对齐到 UTF-8 字符边界
    let mut cut = log.len() - LOG_MAX_BYTES;
    while cut < log.len() && !log.is_char_boundary(cut) {
        cut += 1;
    }
    // 尽量丢到行首，别把一行切成两半。
    // 只在对齐点后面还有正文时才跳，否则会把日志整个清空。
    if let Some(nl) = log[cut..].find('\n') {
        let after_line = cut + nl + 1;
        if after_line < log.len() {
            cut = after_line;
        }
    }
    // 到这一步 cut 必然 < len：初值为 len - LOG_MAX_BYTES 且只有 len > 上限才进来；
    // 对齐字符边界最多前进 3 字节（UTF-8 字符上限 4 字节）；
    // 换行跳转有 after_line < len 的守卫。
    debug_assert!(cut < log.len(), "cut 必须小于长度，否则 drain 会清空整段日志");
    log.drain(..cut);
}

fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    let candidates = [
        "C:\\Windows\\Fonts\\msyh.ttc",
        "C:\\Windows\\Fonts\\msyh.ttf",
        "C:\\Windows\\Fonts\\simhei.ttf",
    ];
    for path in candidates {
        if let Ok(bytes) = std::fs::read(path) {
            // 中文基线修正：雅黑的字形框比拉丁字体高，
            // 不 tweak 的话中文会整体偏下，和按钮/输入框的垂直居中对不上。
            // y_offset_factor 取负值是往上提，-0.08 是实测既不顶格也不下沉的量。
            let data = egui::FontData::from_owned(bytes).tweak(egui::FontTweak {
                scale: 1.0,
                y_offset_factor: -0.08,
                y_offset: 0.0,
                ..Default::default()
            });
            fonts.font_data.insert("cn".into(), std::sync::Arc::new(data));
            if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Proportional) {
                fam.insert(0, "cn".into());
            }
            if let Some(fam) = fonts.families.get_mut(&egui::FontFamily::Monospace) {
                fam.push("cn".into());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        // 界面绘制全部在 ui.rs：这里只做转发。
        // 拆出去是为了让「布局」和「通信/状态」两件事各自可读，
        // 也让 main.rs 不再是一个 400 多行的巨型函数。
        self.draw(ui);
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    /// 验收第 10 条：闸门的四类错误各自有明确文案。
    ///
    /// 这些断言同时锁死「按钮禁用了但逻辑层没拦」这种半吊子实现 ——
    /// `validate()` 必须能独立拒绝，不能只靠界面上的 enabled。
    #[test]
    fn 区域闸门四类错误各自可判() {
        let mut f = CriticalForm::new(
            "修改区域码".into(),
            vec!["upgradetest sdefconf $目标区域码".into()],
            None,
        );
        // 1) 没备份
        assert!(f.validate().is_err());
        assert!(f.validate().unwrap_err().contains("备份"));

        // 备份完成，但没勾选
        f.backup_path = Some("x".into());
        f.current_detected = Some("305".into());
        f.current_input = "305".into();
        f.target_input = "210".into();
        f.target_confirm = "210".into();
        let e = f.validate().unwrap_err();
        assert!(e.contains("勾选"), "实际: {e}");

        // 2) 勾满但两次目标码不一致
        f.ack_reboot = true;
        f.ack_config = true;
        f.ack_register = true;
        f.target_confirm = "211".into();
        let e = f.validate().unwrap_err();
        assert!(e.contains("不一致"), "实际: {e}");

        // 3) 当前码与设备读到的不符
        f.target_confirm = "210".into();
        f.current_input = "999".into();
        let e = f.validate().unwrap_err();
        assert!(e.contains("不一致"), "实际: {e}");

        // 4) 非三位数字
        f.current_input = "305".into();
        f.target_input = "21".into();
        f.target_confirm = "21".into();
        let e = f.validate().unwrap_err();
        assert!(e.contains("三位"), "实际: {e}");

        // 全部正确才通过
        f.target_input = "210".into();
        f.target_confirm = "210".into();
        assert!(f.validate().is_ok());
    }

    /// 没备份就不许执行；勾不满也不许执行。
    #[test]
    fn 区域闸门未完成前置步骤时不可执行() {
        let mut f = CriticalForm::new("x".into(), vec![], None);
        assert!(!f.can_execute(), "没备份就不该可执行");
        f.backup_path = Some("x".into());
        assert!(!f.can_execute(), "备份了但没勾选，仍不该可执行");
        f.ack_reboot = true;
        f.ack_config = true;
        assert!(!f.can_execute(), "只勾了两条，仍不该可执行");
        f.ack_register = true;
        assert!(f.can_execute(), "三条齐了才可执行");
    }

    /// 目标码与当前码相同属于无效操作，必须挡住。
    #[test]
    fn 区域闸门拒绝目标与当前相同() {
        let mut f = CriticalForm::new("x".into(), vec![], None);
        f.backup_path = Some("x".into());
        f.ack_reboot = true;
        f.ack_config = true;
        f.ack_register = true;
        f.current_detected = Some("305".into());
        f.current_input = "305".into();
        f.target_input = "305".into();
        f.target_confirm = "305".into();
        let e = f.validate().unwrap_err();
        assert!(e.contains("相同"), "实际: {e}");
    }

    /// 从 `cat /userconfig/flag_type` 的输出里抠当前区域码。
    #[test]
    fn 能从flag_type输出里抠出区域码() {
        let log = "[已就绪]\n> cat /userconfig/flag_type\ncurrent :  334\n----- 完成 -----";
        assert_eq!(CriticalForm::parse_region(log), Some("334".into()));

        let log2 = "> cat /userconfig/flag_type\ncurrent : 305\n";
        assert_eq!(CriticalForm::parse_region(log2), Some("305".into()));

        // 读不到就返回 None，绝不猜
        assert_eq!(CriticalForm::parse_region("nothing here"), None);
        // 只认 current 打头的键值行：用户在「自定义命令」里敲的
        // `echo current 305` 不能被当成设备读数（否则「必须与设备一致」
        // 这道校验会被一个假值满足）
        assert_eq!(
            CriticalForm::parse_region("> echo current 305\n305"),
            None,
            "含 current 的普通输出不能被当成 flag_type 读数"
        );
        // 真实的等价写法仍然认：等号、大小写、全角冒号两侧的空白
        assert_eq!(CriticalForm::parse_region("current=210"), Some("210".into()));
        assert_eq!(CriticalForm::parse_region("Current: 207"), Some("207".into()));
        // 位数不对也不认
        assert_eq!(
            CriticalForm::parse_region("current : 12"),
            None
        );
    }

    /// 交叉验证用的基准值，由 PowerShell 的
    /// `[datetime]::UnixEpoch.AddSeconds(n).ToUniversalTime().AddHours(8)`
    /// 独立算出，不是拿实现自己对自己。
    #[test]
    fn Unix秒换算北京时间与独立基准一致() {
        let cases = [
            (0u64, (1970, 1, 1, 8, 0, 0)),
            (86_400, (1970, 1, 2, 8, 0, 0)),
            (951_782_400, (2000, 2, 29, 8, 0, 0)), // 闰日
            (1_735_689_600, (2025, 1, 1, 8, 0, 0)),
            (1_760_420_937, (2025, 10, 14, 13, 48, 57)),
            (4_102_444_800, (2100, 1, 1, 8, 0, 0)), // 2100 非闰年
        ];
        for (secs, want) in cases {
            let got = unix_to_local_beijing(secs);
            assert_eq!(got, want, "Unix {secs} 换算不符");
        }
    }

    /// 没登记的占位命令必须被拦下，不能原样发到设备。
    ///
    /// 旧代码只在 `expand_placeholders` 里比对 `__SET_TIME_NOW__` 这一个字符串：
    /// 预设里手滑写成 `__SETTIME_NOW__`，或者新加了占位符却忘了接处理逻辑，
    /// 它会**原样下发**，设备回一句 `sh: ...: not found`，
    /// 而界面照样打「----- 完成 -----」，用户以为生效了。
    #[test]
    fn 未登记的占位命令被拦下() {
        let ok = vec!["sendcmd 1 DB save".to_string(), "reboot".to_string()];
        assert!(unexpanded_placeholder(&ok).is_none(), "普通命令不该被拦");

        // 已登记的那一个不算「未展开」—— 它在 expand_placeholders 里处理
        let known = vec!["__SET_TIME_NOW__".to_string()];
        assert!(
            unexpanded_placeholder(&known).is_none(),
            "已登记的占位命令不该被这里拦下"
        );

        for bad in [
            "__SETTIME_NOW__",          // 少一个下划线
            "sendcmd 1 DB set X 0 F __FOO__",
            "reboot __OPS__",
        ] {
            let cmds = vec![bad.to_string()];
            assert_eq!(
                unexpanded_placeholder(&cmds).map(|s| s.as_str()),
                Some(bad),
                "{bad} 应当被拦下"
            );
        }
    }

    /// 登记了、但没独占一条命令的占位符，同样不许下发。
    ///
    /// 这是上一轮修复的窄化版本：校验按 token 判断「登记过没有」，
    /// 替换却按「整串相等」判断 —— 于是 `date -s __SET_TIME_NOW__`
    /// 既能通过校验，又不会被替换，字面量就这么发到了设备上。
    /// 两处口径必须完全一致。
    #[test]
    fn 复合命令里的占位符也算未展开() {
        // 登记过的占位符，独占一条命令 → 会被替换，放行
        let ok = vec!["__SET_TIME_NOW__".to_string()];
        assert!(
            unexpanded_placeholder(&ok).is_none(),
            "独占一条命令的已登记占位符应当放行"
        );
        // 替换后确实是真实命令
        let out = expand_placeholders(&ok);
        assert!(out[0].starts_with("date -s "), "应当被替换成校时命令");

        // 同一个占位符塞进复合命令 → 替换逻辑不管它，必须拦下
        for bad in [
            "date -s __SET_TIME_NOW__",
            "echo __SET_TIME_NOW__",
            "  __SET_TIME_NOW__  x",
        ] {
            let cmds = vec![bad.to_string()];
            assert_eq!(
                unexpanded_placeholder(&cmds).map(|s| s.as_str()),
                Some(bad),
                "{bad:?} 会被原样下发，必须拦下"
            );
        }
    }

    /// 展开与拦截必须对同一份输入给出一致的判断：
    /// 「拦下」当且仅当「展开后仍然留着占位符」。
    ///
    /// 这条是把两个函数的口径钉在一起 —— 以后谁单方面改了其中一边，
    /// 缝隙就会重新出现。
    #[test]
    fn 拦截与展开的口径一致() {
        let cases = [
            "__SET_TIME_NOW__",
            "date -s __SET_TIME_NOW__",
            "__SETTIME_NOW__",
            "sendcmd 1 DB set X 0 F __FOO__",
            "sendcmd 1 DB save",
            "reboot",
        ];
        for c in cases {
            let cmds = vec![c.to_string()];
            let blocked = unexpanded_placeholder(&cmds).is_some();
            let expanded = expand_placeholders(&cmds);
            let leftover = expanded[0].split_whitespace().any(looks_like_placeholder);
            assert_eq!(
                blocked, leftover,
                "{c:?}: 拦截={blocked}，展开后残留={leftover}，两边口径不一致"
            );
        }
    }

    /// 内置清单里出现的占位命令必须全部登记在 `PLACEHOLDERS` 里。
    ///
    /// 这条守的是「以后新增占位符时忘了登记」的退化：
    /// 忘了登记不会编译报错，只会在用户点下去的那一刻变成一条 not found。
    #[test]
    fn 内置清单里的占位命令都已登记() {
        let mut seen = 0usize;
        for (_g, actions) in zte_g7615_panel::presets::builtin() {
            for a in actions {
                for c in &a.cmds {
                    for tok in c.split_whitespace() {
                        if tok.starts_with("__") && tok.ends_with("__") {
                            assert!(
                                PLACEHOLDERS.contains(&tok),
                                "{} 里的占位命令 {tok} 没登记在 PLACEHOLDERS",
                                a.name
                            );
                            seen += 1;
                        }
                    }
                }
            }
        }
        assert!(seen >= 1, "内置清单应至少有一处占位命令，否则这条测试没意义");
    }

    /// 日志裁剪：必须在 UTF-8 字符边界与行首处切，绝不能 panic、绝不能切半个汉字。
    ///
    /// 以前这段逻辑埋在 `append_log` 里（要写文件、碰 `log_file`），
    /// 测试根本驱动不了，等于这个「会在字符边界上切字符串」的地方从没被验证过。
    #[test]
    fn 日志裁剪在字符边界与行首处切() {
        // 构造一段「行很短、全是中文」的日志，让裁剪点恰好落进一个汉字中间
        let line = "中文日志测试\n";
        let mut log = String::new();
        while log.len() <= LOG_MAX_BYTES + 4096 {
            log.push_str(line);
        }
        let before_len = log.len();
        trim_log(&mut log);
        assert!(log.len() < before_len, "超限后应当被裁剪");
        assert!(
            log.len() <= LOG_MAX_BYTES,
            "裁剪后仍是 {} 字节，超过上限 {LOG_MAX_BYTES}",
            log.len()
        );
        // 不 panic 已经由「跑到了这里」证明；再验一遍切点在行首、不是半个字
        assert!(
            log.starts_with("中文日志测试\n"),
            "切点不该落在行中间: {:?}",
            &log[..log.len().min(12)]
        );
        assert!(log.is_char_boundary(0));
    }

    /// 轮转阈值：超限才轮转，刚好等于上限时不动。
    ///
    /// 写死成 true 会让每行都轮转（日志等于没有），写死成 false
    /// 会让单个文件无限长大 —— 两者都不会编译报错。
    #[test]
    fn 日志轮转阈值() {
        assert!(!should_rotate_log(0), "空文件不该轮转");
        assert!(!should_rotate_log(LOG_FILE_MAX_BYTES), "刚好等于上限时不该轮转");
        assert!(should_rotate_log(LOG_FILE_MAX_BYTES + 1), "超一个字节就该轮转");
        assert!(LOG_FILE_MAX_BYTES >= 1024 * 1024, "上限不该小于 1MB");
        assert!(LOG_FILE_MAX_BYTES <= 64 * 1024 * 1024, "上限不该大于 64MB");
    }

    /// 日志文件超限后必须轮转：当前文件让位成 .old.txt，新的从空开始。
    ///
    /// 内存那份裁到 256KB，落盘那份以前没有上限 —— 一次长会话能把单个
    /// 文件写到几十 MB，而清理只在下次启动时发生。
    #[test]
    fn 日志文件超限后轮转() {
        let dir = std::env::temp_dir().join(format!("zte_rotate_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("运行日志_20260101_000000.txt");
        std::fs::write(&path, "第一份内容\n").unwrap();

        rotate_log_file(&path).expect("轮转应当成功");
        // 当前文件已经不在了（下一次 append 会重新创建）
        assert!(!path.exists(), "轮转后当前文件应当让位");
        let old = path.with_extension("old.txt");
        assert!(old.exists(), "应当留下 .old.txt");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "第一份内容\n");

        // 再轮转一次：旧的 .old 被覆盖，不该因为目标存在而失败
        std::fs::write(&path, "第二份内容\n").unwrap();
        rotate_log_file(&path).expect("第二次轮转也应当成功");
        assert_eq!(std::fs::read_to_string(&old).unwrap(), "第二份内容\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 没超上限时一个字节都不能动。
    #[test]
    fn 日志未超限时不被裁剪() {
        let mut log = String::from("短日志\n");
        trim_log(&mut log);
        assert_eq!(log, "短日志\n");
    }

    /// 占位符必须被替换，绝不能原样下发。
    #[test]
    fn 占位符被替换成真实命令() {
        let cmds = vec!["__SET_TIME_NOW__".to_string()];
        let out = expand_placeholders(&cmds);
        assert_eq!(out.len(), 1);
        assert!(out[0].starts_with("date -s "), "未替换: {}", out[0]);
        assert!(!out[0].contains("__"), "替换后残留占位符: {}", out[0]);
    }

    /// 普通命令原样通过，不被改动。
    #[test]
    fn 非占位命令不被改动() {
        let cmds = vec![
            "sendcmd 1 DB p DevInfo".to_string(),
            "reboot".to_string(),
        ];
        assert_eq!(expand_placeholders(&cmds), cmds);
    }

    /// 校时命令里的时间必须是 UTC（比北京慢 8 小时），
    /// 否则设备时间会偏差 8 小时。
    #[test]
    fn 校时命令下发的是UTC时间() {
        // 固定一个已知时刻来验证换算方向
        let beijing = unix_to_local_beijing(1_760_420_937);
        assert_eq!(beijing, (2025, 10, 14, 13, 48, 57), "先确认北京时刻");
        let cmd = set_time_cmd_from_beijing(beijing.0, beijing.1, beijing.2, beijing.3, beijing.4, beijing.5);
        // 13:48 北京 → 05:48 UTC
        assert_eq!(cmd, "date -s \"2025-10-14 05:48:57\"");
    }
}

/// 截图模式：`--screenshot <输出.png>`。
///
/// 为什么要有这个模式：这台机器上的会话是锁屏状态（LogonUI 在跑），
/// 屏幕抓取拿到的是锁屏壁纸而不是窗口内容，GDI 那条路走不通。
/// 用 egui 自己的 `ViewportCommand::Screenshot` 取的是渲染结果本身，
/// 与屏幕是否可见无关 —— 验收要求的「改造后截图」只能这么拿。
///
/// 它只影响启动方式，不改变任何业务逻辑：正常双击运行时根本不会走到这里。
fn run_screenshot_mode(
    out_path: std::path::PathBuf,
    force_width: Option<f32>,
    force_sidebar: Option<f32>,
    open_groups: bool,
    demo: bool,
    demo_pending: bool,
    theme_pref: egui::ThemePreference,
    critical_demo: bool,
) -> eframe::Result<()> {
    use std::sync::{Arc, Mutex};

    // 默认按设计尺寸出图；传了 --width 就按该宽度出，
    // 高度仍按 φ 推导（宽度 / 1.618），保证任何宽度下都符合黄金比例。
    let width = force_width.unwrap_or(theme::WINDOW_DEFAULT_W);
    let height = (width / 1.618).round();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([width, height])
            .with_min_inner_size([theme::WINDOW_MIN_W, theme::WINDOW_MIN_H]),
        ..Default::default()
    };

    let mut app = App::new_headless();
    app.theme_pref = theme_pref;
    if open_groups {
        app.open_all_groups = true;
    }
    if let Some(w) = force_sidebar {
        app.sidebar_width = w;
    }
    if demo {
        app.seed_demo_state(demo_pending);
    }
    if critical_demo {
        // 灌一份「刚备份完、三项已勾、码也填好」的闸门状态，
        // 好让这个平时要走完整流程才能看到的界面能被截图验收。
        app.seed_critical_demo();
    }
    let captured: Arc<Mutex<Option<egui::ColorImage>>> = Arc::new(Mutex::new(None));
    let captured_in = captured.clone();
    let out_for_closure = out_path.clone();

    eframe::run_native(
        "中兴光猫控制面板 - Telnet",
        options,
        Box::new(move |cc| {
            install_fonts(&cc.egui_ctx);
            theme::install(&cc.egui_ctx, theme_pref);
            Ok(Box::new(ScreenshotApp {
                app,
                captured: captured_in.clone(),
                out: out_for_closure.clone(),
                started: std::time::Instant::now(),
                requested: false,
            }))
        }),
    )?;

    // run_native 在窗口关闭后返回；文件在捕获那一帧就已经写盘了
    if !out_path.exists() {
        eprintln!("截图失败：没有拿到任何一帧");
    }
    Ok(())
}

/// 窗口出现后至少要等多久才截图（毫秒）。
///
/// **必须按墙钟时间等，不能按帧数等**：egui 的弹出层淡入用的是
/// `age`（真实经过的秒数）对照 `animation_time`（0.12s），
/// 而截图模式是无头连续出帧，16 帧可能只花了 10 毫秒 ——
/// 动画进度还在 8%，截出来的模态是半透明的。
/// 取 400ms：淡入 120ms 走完，再留 3 倍余量给首帧字体上传与布局稳定。
const SNAPSHOT_DELAY: std::time::Duration = std::time::Duration::from_millis(400);

/// 截图模式专用的 App：先渲染几帧让布局稳定，再要一张截图。
struct ScreenshotApp {
    app: App,
    captured: std::sync::Arc<std::sync::Mutex<Option<egui::ColorImage>>>,
    out: std::path::PathBuf,
    started: std::time::Instant,
    requested: bool,
}

impl eframe::App for ScreenshotApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.app.draw(ui);

        // 等到时再截图：早了会截到淡入中的半透明弹窗，也截不到稳定布局。
        let ready = self.started.elapsed() >= SNAPSHOT_DELAY;
        if ready && !self.requested {
            self.requested = true;
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }

        let img = ui.ctx().input(|i| {
            i.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = img {
            let saved = save_color_image(&self.out, &img);
            *self.captured.lock().unwrap() = Some((*img).clone());
            if let Err(e) = saved {
                eprintln!("截图写盘失败：{e}");
            }
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        } else {
            // 等截图回包期间必须持续重绘，否则事件驱动模式下会一直停住
            ui.ctx().request_repaint();
        }
    }
}

/// 把 egui 的 ColorImage 写成 PNG。
///
/// 不引 image crate：项目禁止新增依赖，而 PNG 的「无压缩滤波 + zlib 存储块」
/// 用标准库就能写出来，对一张 1280×792 的截图完全够用。
fn save_color_image(path: &std::path::Path, img: &egui::ColorImage) -> std::io::Result<()> {
    let (w, h) = (img.width(), img.height());
    let mut raw: Vec<u8> = Vec::with_capacity(h * (1 + w * 4));
    for y in 0..h {
        raw.push(0); // 滤波类型 0 = None
        for x in 0..w {
            let c = img.pixels[y * w + x];
            raw.extend_from_slice(&[c.r(), c.g(), c.b(), c.a()]);
        }
    }

    // zlib 容器：2 字节头 + deflate 存储块 + adler32
    let mut z = Vec::new();
    z.extend_from_slice(&[0x78, 0x01]);
    let mut i = 0usize;
    while i < raw.len() {
        let chunk = (raw.len() - i).min(0xFFFF);
        let last = if i + chunk == raw.len() { 1u8 } else { 0u8 };
        z.push(last);
        z.extend_from_slice(&(chunk as u16).to_le_bytes());
        z.extend_from_slice(&(!(chunk as u16)).to_le_bytes());
        z.extend_from_slice(&raw[i..i + chunk]);
        i += chunk;
    }
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in &raw {
        a = (a + byte as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());

    let mut png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    let mut chunk = |kind: &[u8], data: &[u8]| {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        png.extend_from_slice(&body);
        png.extend_from_slice(&crc32(&body).to_be_bytes());
    };
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8bit / RGBA / deflate / 无滤波 / 不隔行
    chunk(b"IHDR", &ihdr);
    chunk(b"IDAT", &z);
    chunk(b"IEND", &[]);

    std::fs::write(path, png)
}

/// 标准 PNG CRC-32。
fn crc32(data: &[u8]) -> u32 {
    let mut table = [0u32; 256];
    for (n, slot) in table.iter_mut().enumerate() {
        let mut c = n as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
        *slot = c;
    }
    let mut crc = 0xFFFF_FFFFu32;
    for &b in data {
        crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    crc ^ 0xFFFF_FFFF
}

fn main() -> eframe::Result<()> {
    // 主线程栈已在 build.rs 里通过链接参数加大到 16MB：
    // Windows 默认只有 1MB，eframe/wgpu 初始化会把 1MB 用尽并直接崩溃
    // （thread 'main' has overflowed its stack）。
    // 这里必须留在主线程：winit 在 Windows 上拒绝在非主线程建事件循环。
    //
    // 截图模式：`--screenshot <文件>`，供验收时取渲染结果用。
    // 正常双击运行不会带这个参数，走的还是下面这条路径。
    // 三个参数都是验收用的，正常双击运行时一律不生效：
    //   --screenshot <文件>   把当前渲染结果存成 PNG 后退出
    //   --width <像素>        指定窗口宽度（最小窗口适配验收用）
    //   --open-groups         启动时展开全部分组（截图里能看到操作行与危险标记）
    let args: Vec<String> = std::env::args().collect();
    let arg_value = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let force_width = arg_value("--width").and_then(|v| v.parse::<f32>().ok());
    let force_sidebar = arg_value("--sidebar").and_then(|v| v.parse::<f32>().ok());
    let open_groups = args.iter().any(|a| a == "--open-groups");
    // --demo 只给验收截图用：灌一份假状态（已连接 / 若干回读校验 / 一段日志），
    // 让「连接后才有内容」的控件也能被截到。它不发任何命令、不连任何设备。
    let demo = args.iter().any(|a| a == "--demo");
    let demo_pending = args.iter().any(|a| a == "--demo-pending");
    // --theme light|dark：截图时指定主题，两种都要能出图（验收要求）。
    // 不传就按外观设置文件来，与正常运行一致。
    let theme_pref: egui::ThemePreference = match arg_value("--theme").as_deref() {
        Some("light") => egui::ThemePreference::Light,
        Some("dark") => egui::ThemePreference::Dark,
        Some("system") => egui::ThemePreference::System,
        _ => zte_g7615_panel::appearance::load().into(),
    };
    // --critical-demo：把改区域闸门填成「可以执行」的样子，供截图验收。
    let critical_demo = args.iter().any(|a| a == "--critical-demo");

    if let Some(out) = arg_value("--screenshot") {
        return run_screenshot_mode(
            std::path::PathBuf::from(out),
            force_width,
            force_sidebar,
            open_groups,
            demo,
            demo_pending,
            theme_pref,
            critical_demo,
        );
    }

    // 尺寸全部来自 theme：1280×792（宽/φ）与 1000×618（宽/φ），
    // 与 docs/设计规范.md 第 3 节一致。
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([theme::WINDOW_DEFAULT_W, theme::WINDOW_DEFAULT_H])
            .with_min_inner_size([theme::WINDOW_MIN_W, theme::WINDOW_MIN_H]),
        ..Default::default()
    };
    eframe::run_native(
        "中兴光猫控制面板 - Telnet",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}
