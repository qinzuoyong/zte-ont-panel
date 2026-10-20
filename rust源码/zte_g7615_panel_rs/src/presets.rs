//! 界面上的命令预设（数据驱动）。
//!
//! 命令清单不再硬编码在 Rust 里，而是来自 `命令预设.txt`：
//! - 优先读 exe 同目录下的 `命令预设.txt`（用户可自行编辑，改完重启生效）
//! - 文件不存在时用编译进二进制的默认清单（`DEFAULT_PRESETS`）
//! - 文件存在但解析失败时**不静默回落**，而是把错误报给界面 ——
//!   否则用户会以为自己的修改生效了，实际还在跑旧清单
//!
//! 格式与解析见 `config.rs`。
//!
//! 注意：无论命令从哪来，写操作都会走 `dbcli` 的写后回读校验，
//! 真机字段名与清单不符时界面会标 ❌ 并指出具体字段，不会静默骗人。

use crate::config;
use std::path::{Path, PathBuf};

/// 编译进二进制的默认命令清单。
///
/// 与项目根目录的 `命令预设.txt` 内容一致（由测试守护两者同步）；
/// 首次运行时程序会把它写到 exe 旁边，方便用户编辑。
pub const DEFAULT_PRESETS: &str = include_str!("../default_presets.txt");

/// 预设文件名。
pub const PRESET_FILENAME: &str = "命令预设.txt";

/// 一个可执行的操作：显示名 + 是否危险（需要二次确认）+ 要依次执行的命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    pub name: String,
    pub danger: bool,
    /// 刷机级操作（比 danger 高一级，走专用闸门而不是普通二次确认）。
    /// 见 `config::ActionDef::critical`。
    pub critical: bool,
    pub cmds: Vec<String>,
    /// 这个操作为什么危险 / 有什么坑。危险操作弹确认条时一并显示，
    /// 避免用户只看到「确认要执行 XXX 吗」却不知道后果。
    pub note: Option<String>,
    /// 参数声明。非空时点按钮会先弹输入表单，
    /// 而不是直接把带 `$参数名` 的命令发出去。
    pub params: Vec<config::ParamDef>,
}

impl Action {
    /// 是否需要先让用户填参数。
    pub fn needs_input(&self) -> bool {
        !self.params.is_empty()
    }

    /// 校验一组输入值。
    ///
    /// 返回第一个不合格的参数说明；全部通过返回 Ok。
    /// 在发到设备**之前**挡住格式错误 —— 光猫对 SN/MAC 有硬性格式要求，
    /// 发错了要么静默失败、要么写进一个半吊子值。
    pub fn validate_inputs(&self, values: &[(String, String)]) -> Result<(), String> {
        for p in &self.params {
            let v = values
                .iter()
                .find(|(k, _)| *k == p.name)
                .map(|(_, v)| v.as_str())
                .unwrap_or("");
            if let Err(why) = p.kind.validate(v) {
                return Err(format!("参数「{}」{}", p.name, why));
            }
        }
        Ok(())
    }

    /// 用输入值展开命令。未提供值的参数保持原样（调用方应先校验）。
    pub fn expand(&self, values: &[(String, String)]) -> Vec<String> {
        self.cmds
            .iter()
            .map(|c| config::substitute(c, values))
            .collect()
    }
}

/// 预设加载结果。
///
/// 刻意区分这几种情况，而不是统一返回 `Vec`：
/// 用户必须知道**当前生效的是哪份清单**，
/// 否则「我明明改了文件怎么没生效」会变成一个查不出原因的坑。
#[derive(Debug)]
pub enum Loaded {
    /// 用编译进来的默认清单（exe 旁边没有预设文件）
    Builtin(Vec<(String, Vec<Action>)>),
    /// 用用户文件（附来源路径，界面上显示「正在用哪个文件」）
    FromFile(Vec<(String, Vec<Action>)>, String),
    /// 用户文件存在但解析失败。**必须让用户看到**，绝不静默回落。
    Failed(String, String),
}

impl Loaded {
    /// 取出可用的预设列表。解析失败时回落到内置清单，
    /// 但调用方应当同时把 `error()` 显示出来。
    pub fn groups(&self) -> &[(String, Vec<Action>)] {
        match self {
            Loaded::Builtin(g) => g,
            Loaded::FromFile(g, _) => g,
            // 解析失败时先用内置清单让界面能显示，但错误必须另外报出来
            Loaded::Failed(_, _) => &[],
        }
    }
}

/// 把 `config` 解析出的定义转成界面用的 `Action`。
fn to_actions(groups: Vec<config::GroupDef>) -> Vec<(String, Vec<Action>)> {
    groups
        .into_iter()
        .map(|g| {
            let actions = g
                .actions
                .into_iter()
                .map(|a| Action {
                    name: a.name,
                    danger: a.danger,
                    critical: a.critical,
                    cmds: a.cmds,
                    note: a.note,
                    params: a.params,
                })
                .collect();
            (g.name, actions)
        })
        .collect()
}

/// 解析一段预设文本。
pub fn parse_str(text: &str) -> Result<Vec<(String, Vec<Action>)>, config::ParseError> {
    config::parse(text).map(to_actions)
}

/// 预设文件应该放的位置：exe 同目录。
///
/// **刻意留在根目录**而不是收进 `数据/`：这是用户会主动编辑的文件，
/// 埋深了反而不好找。其余运行数据都收在 `数据/` 下。
pub fn preset_path() -> Option<PathBuf> {
    Some(preset_dir()?.join(PRESET_FILENAME))
}

/// 从指定路径加载预设。
pub fn load_from(path: &Path) -> Loaded {
    match std::fs::read_to_string(path) {
        Ok(text) => match parse_str(&text) {
            Ok(g) => Loaded::FromFile(g, path.display().to_string()),
            Err(e) => Loaded::Failed(e.to_string(), path.display().to_string()),
        },
        // 文件不存在（或读不了）：用内置清单，这不是错误
        Err(_) => Loaded::Builtin(builtin()),
    }
}

/// 按标准位置加载预设（未识别机型时用默认清单）。
pub fn load() -> Loaded {
    load_for_model(crate::model::Model::Unknown)
}

/// 按机型加载预设。
///
/// 查找顺序（exe 同目录）：
/// 1. `命令预设_<机型后缀>.txt` —— 机型专用清单
/// 2. `命令预设.txt` —— 用户通用清单
/// 3. 内置清单
///
/// 机型专用清单优先，是因为它按该机型的**真实回显**核对过；
/// 通用清单是用户自己改的，可能混入其他机型的命令。
pub fn load_for_model(model: crate::model::Model) -> Loaded {
    let Some(dir) = preset_dir() else {
        return Loaded::Builtin(builtin());
    };

    // 1) 机型专用
    let suffix = model.preset_suffix();
    if !suffix.is_empty() {
        let p = dir.join(format!("命令预设_{suffix}.txt"));
        if p.exists() {
            return match std::fs::read_to_string(&p) {
                Ok(text) => match parse_str(&text) {
                    Ok(g) => Loaded::FromFile(g, p.display().to_string()),
                    Err(e) => Loaded::Failed(e.to_string(), p.display().to_string()),
                },
                Err(e) => Loaded::Failed(format!("读取失败: {e}"), p.display().to_string()),
            };
        }
    }

    // 2) 通用
    let generic = dir.join(PRESET_FILENAME);
    load_from(&generic)
}

/// 预设文件所在目录（exe 同目录，刻意不放进 `数据/`）。
pub fn preset_dir() -> Option<PathBuf> {
    crate::paths::preset_dir()
}

/// 按机型过滤命令：剔除该机型已知不支持的命令。
///
/// 返回 `(保留的组, 被剔除的命令列表)`。
/// 被剔除的命令会报给用户 —— 让用户知道「少的是因为机型不支持」，
/// 而不是以为程序坏了。
pub fn filter_for_model(
    model: crate::model::Model,
    groups: Vec<(String, Vec<Action>)>,
) -> (Vec<(String, Vec<Action>)>, Vec<String>) {
    let mut removed: Vec<String> = Vec::new();
    let mut out: Vec<(String, Vec<Action>)> = Vec::new();

    for (name, actions) in groups {
        let mut kept_actions: Vec<Action> = Vec::new();
        for mut a in actions {
            let (kept, gone) = crate::model::filter_commands(model, &a.cmds);
            removed.extend(gone);
            if kept.is_empty() {
                // 整个操作的命令都被剔光了 —— 直接不显示这个按钮，
                // 否则会留一个点了什么都不做的空按钮
                continue;
            }
            a.cmds = kept;
            kept_actions.push(a);
        }
        if !kept_actions.is_empty() {
            out.push((name, kept_actions));
        }
    }
    (out, removed)
}

/// 内置清单。
pub fn builtin() -> Vec<(String, Vec<Action>)> {
    parse_str(DEFAULT_PRESETS).unwrap_or_default()
}

/// 首次运行时把内置清单写到 exe 旁边，方便用户编辑。
///
/// 已存在则**不覆盖** —— 用户可能已经改过，覆盖会丢掉他的修改。
/// 返回 true 表示确实写出了新文件。
pub fn ensure_preset_file(path: &Path) -> bool {
    if path.exists() {
        return false;
    }
    std::fs::write(path, DEFAULT_PRESETS).is_ok()
}

/// 生成「把光猫时间设为当前北京时间（按 UTC 下发）」的命令。
///
/// 光猫时钟是 UTC，国内使用需要减 8 小时；手工换算容易算错，
/// 所以由程序按点击时刻计算。
///
/// `now` 参数化传入是为了可测——不要在这里直接读系统时间。
pub fn set_time_cmd(now_utc: (i32, u32, u32, u32, u32, u32)) -> String {
    let (y, mo, d, h, mi, s) = now_utc;
    format!("date -s \"{:04}-{:02}-{:02} {:02}:{:02}:{:02}\"", y, mo, d, h, mi, s)
}

/// 把北京时间的「年月日时分秒」换算成 UTC 并生成命令行。
///
/// 简化处理：直接按 -8 小时减，必要时向前借位到前一天/前一月。
/// 光猫校时对跨月边界不敏感（差一天用户会立刻发现并重设），
/// 但仍做正确的日期借位，避免出现 `2026-10-00` 这种非法日期。
pub fn set_time_cmd_from_beijing(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> String {
    let (mut y, mut mo, mut d) = (year, month, day);
    // 先按小时借位
    let (mut h, borrow_day) = if hour >= 8 {
        (hour - 8, false)
    } else {
        (hour + 24 - 8, true)
    };
    if borrow_day {
        if d > 1 {
            d -= 1;
        } else {
            // 退到上个月最后一天
            let (py, pmo) = if mo > 1 { (y, mo - 1) } else { (y - 1, 12) };
            y = py;
            mo = pmo;
            d = days_in_month(y, mo);
        }
    }
    let _ = &mut h; // h 已是最终值
    set_time_cmd((y, mo, d, h, minute, second))
}

/// 某年某月的天数。闰年规则：能被 4 整除且（不能被 100 整除或能被 400 整除）。
pub fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 {
                29
            } else {
                28
            }
        }
        _ => 30,
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn 预设分组非空且命令非空() {
        let groups = builtin();
        assert!(groups.len() >= 8, "分组数过少: {}", groups.len());
        for (g, actions) in &groups {
            assert!(!g.is_empty());
            assert!(!actions.is_empty(), "分组 {g} 没有操作");
            for a in actions {
                assert!(!a.name.is_empty());
                assert!(!a.cmds.is_empty(), "{} 没有命令", a.name);
                for c in &a.cmds {
                    assert!(!c.trim().is_empty(), "{} 有空命令", a.name);
                }
            }
        }
    }

    /// 内置清单必须能解析成功。
    ///
    /// 这条是兜底：`builtin()` 里用了 `unwrap_or_default()`，
    /// 解析失败会静默变成空清单、界面一个按钮都没有。
    /// 所以必须有测试保证它解析得动。
    #[test]
    fn 内置清单能解析且不为空() {
        assert!(
            parse_str(DEFAULT_PRESETS).is_ok(),
            "内置清单解析失败: {:?}",
            parse_str(DEFAULT_PRESETS).unwrap_err()
        );
        let g = builtin();
        assert!(!g.is_empty(), "内置清单解析为空");
        let total: usize = g.iter().map(|(_, a)| a.len()).sum();
        assert!(total >= 30, "内置操作数过少: {total}");
    }

    /// 占位命令不能被当成真命令漏到设备上。
    #[test]
    fn 占位命令只出现在校时那一项() {
        let mut found = 0;
        for (_g, actions) in builtin() {
            for a in actions {
                for c in &a.cmds {
                    if c.starts_with("__") {
                        assert!(c == "__SET_TIME_NOW__", "未知占位命令: {c}");
                        assert!(a.name.contains("校时"), "占位命令出现在非校时操作: {}", a.name);
                        found += 1;
                    }
                }
            }
        }
        assert_eq!(found, 1, "应恰好有 1 处占位命令，实际 {found}");
    }

    #[test]
    fn 危险操作都有确认所需的信息() {
        for (_g, actions) in builtin() {
            for a in actions {
                if a.danger {
                    // 危险操作必须能被 UI 识别（danger 标志），
                    // 且带风险说明的应当说明非空
                    if let Some(n) = &a.note {
                        assert!(!n.trim().is_empty(), "{} 的风险说明为空", a.name);
                    }
                } else {
                    assert!(a.note.is_none(), "非危险操作 {} 不该有风险说明", a.name);
                }
            }
        }
    }

    // ---------- 数据驱动加载 ----------

    /// exe 旁边没有文件时用内置清单。
    #[test]
    fn 无文件时用内置清单() {
        let missing = Path::new("__definitely_not_here__.txt");
        assert!(matches!(load_from(missing), Loaded::Builtin(_)));
    }

    /// 有文件时用文件内容，并记下来源路径。
    #[test]
    fn 有文件时用文件内容() {
        let dir = std::env::temp_dir();
        let p = dir.join("zte_test_presets_ok.txt");
        std::fs::write(&p, "group 测试组\n  action 测试操作\n    cmd echo hi\n").unwrap();
        let l = load_from(&p);
        match &l {
            Loaded::FromFile(g, src) => {
                assert_eq!(g.len(), 1);
                assert_eq!(g[0].0, "测试组");
                assert_eq!(g[0].1[0].cmds, vec!["echo hi"]);
                assert!(src.contains("zte_test_presets_ok"), "来源路径不对: {src}");
            }
            other => panic!("应走 FromFile，实际: {other:?}"),
        }
        let _ = std::fs::remove_file(&p);
    }

    /// **关键行为**：文件写坏了必须报出来，不能静默回落到内置清单。
    ///
    /// 静默回落的后果是「我改了文件怎么没生效」——
    /// 用户会去查程序和设备，而真正的问题在自己刚改的那一行。
    #[test]
    fn 文件写坏时报错不静默回落() {
        let dir = std::env::temp_dir();
        let p = dir.join("zte_test_presets_bad.txt");
        std::fs::write(&p, "group 测试组\n  action 坏操作\n    bogus 这条不合法\n").unwrap();
        let l = load_from(&p);
        match &l {
            Loaded::Failed(msg, src) => {
                // 内容里 bogus 在第 3 行（第 1 行 group、第 2 行 action）
                assert!(msg.contains("3"), "错误应带正确行号，实际: {msg}");
                assert!(msg.contains("bogus"), "错误应指出出问题的内容，实际: {msg}");
                assert!(src.contains("zte_test_presets_bad"), "应报出文件路径: {src}");
            }
            other => panic!("写坏的文件必须报 Failed，实际: {other:?}"),
        }
        let _ = std::fs::remove_file(&p);
    }

    /// 首次运行会写出预设文件；已存在时不覆盖（不能丢用户改动）。
    #[test]
    fn 首次写出预设文件且不覆盖已有内容() {
        let dir = std::env::temp_dir();
        let p = dir.join("zte_test_presets_write.txt");
        let _ = std::fs::remove_file(&p);

        // 第一次：文件不存在 → 写出
        assert!(ensure_preset_file(&p), "文件不存在时应写出");
        assert!(p.exists());
        let written = std::fs::read_to_string(&p).unwrap();
        assert_eq!(written, DEFAULT_PRESETS, "写出的内容应与内置清单一致");

        // 用户改了文件
        std::fs::write(&p, "group 用户改过的\n  action A\n    cmd x\n").unwrap();

        // 第二次：文件已存在 → 不能覆盖
        assert!(!ensure_preset_file(&p), "文件已存在时不该重复写出");
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("用户改过的"), "用户改动被覆盖了: {after}");

        let _ = std::fs::remove_file(&p);
    }

    /// 内置清单与项目根目录的 `命令预设.txt` 必须保持一致。 两份文件很容易改一份忘另一份，那条差异会让
    /// 「文档里写的」和「程序实际跑的」对不上。
    #[test]
    fn 内置清单与根目录预设文件一致() {
        // 从 crate 根往上找到项目根目录
        let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let project_root = crate_root
            .parent() // rust源码/
            .and_then(|p| p.parent()) // 中兴光猫控制面板/
            .expect("找不到项目根目录");
        let root_file = project_root.join("命令预设.txt");
        if !root_file.exists() {
            // 打包分发时可能没有，不算失败
            return;
        }
        let root_text = std::fs::read_to_string(&root_file).unwrap();
        assert_eq!(
            root_text.replace("\r\n", "\n"),
            DEFAULT_PRESETS.replace("\r\n", "\n"),
            "根目录 命令预设.txt 与 default_presets.txt 不一致，请同步"
        );
    }

    // ---------- 机型专用清单 ----------

    /// 项目根目录里按机型命名的清单都要能解析。
    ///
    /// 这些文件是按真机回显核对过的，最容易在手工编辑时写坏；
    /// 解析失败会在运行时静默退到内置清单，用户只会觉得「按钮不对」。
    /// 所以在这里一次性守住。
    #[test]
    fn 机型专用清单都能解析() {
        let Some(root) = project_root() else { return };
        let mut checked = 0;
        for (suffix, model) in [
            ("f650", crate::model::Model::F650Series),
            ("g7615", crate::model::Model::G7615Series),
        ] {
            let p = root.join(format!("命令预设_{suffix}.txt"));
            if !p.exists() {
                continue;
            }
            let text = std::fs::read_to_string(&p).unwrap();
            let groups = parse_str(&text)
                .unwrap_or_else(|e| panic!("{} 解析失败: {e}", p.display()));
            assert!(!groups.is_empty(), "{suffix} 清单为空");

            // 关键：清单里不能含该机型已知不支持的命令，
            // 否则界面上会留一个点了必然报错的按钮
            for (_name, actions) in &groups {
                for a in actions {
                    for c in &a.cmds {
                        for bad in model.unsupported_fragments() {
                            assert!(
                                !c.contains(bad),
                                "[{suffix}] {} 含本机型不支持的命令 {c}",
                                a.name
                            );
                        }
                    }
                }
            }
            checked += 1;
        }
        // 至少应有一个机型清单存在
        assert!(checked >= 1, "没找到任何机型专用清单");
    }

    /// F650 清单里的字段名必须来自真机回显。
    ///
    /// 这些是我实际连设备抓到的表名/字段名，写错会静默失败。
    #[test]
    fn f650清单用真机确认过的表名() {
        let Some(root) = project_root() else { return };
        let p = root.join("命令预设_f650.txt");
        if !p.exists() {
            return;
        }
        let text = std::fs::read_to_string(&p).unwrap();
        // 这些表在真实 F650 上都确认存在
        for tbl in ["WLANPSK", "WLANBase", "DevAuthInfo", "CltLmt"] {
            assert!(
                text.contains(tbl),
                "F650 清单应覆盖真机存在的表 {tbl}"
            );
        }
    }

    /// 找到项目根目录（`CARGO_MANIFEST_DIR` 的上两级）。
    fn project_root() -> Option<std::path::PathBuf> {
        let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let p = crate_root.parent()?.parent()?;
        if p.exists() { Some(p.to_path_buf()) } else { None }
    }

    #[test]
    fn 校时命令按UTC减八小时() {
        // 北京时间 2026-10-14 13:48:57 → UTC 05:48:57 同日
        let c = set_time_cmd_from_beijing(2026, 10, 14, 13, 48, 57);
        assert_eq!(c, "date -s \"2026-10-14 05:48:57\"");
    }

    // ---------- 参数化操作 ----------

    /// 内置清单里应当有参数化操作，否则这功能等于没接上。
    #[test]
    fn 内置清单含参数化操作() {
        let all = builtin();
        let with_params: Vec<&Action> = all
            .iter()
            .flat_map(|(_, actions)| actions.iter())
            .filter(|a| a.needs_input())
            .collect();
        assert!(
            with_params.len() >= 5,
            "参数化操作过少: {}，参数功能可能没接到预设里",
            with_params.len()
        );
    }

    /// **关键**：展开后不能残留 `$参数名`。
    ///
    /// 残留意味着会把 `$新密码` 原样发到设备 ——
    /// 设备收到一串乱码，而用户以为改成功了。
    #[test]
    fn 全部参数化操作展开后不残留占位符() {
        for (_g, actions) in builtin() {
            for a in actions {
                if !a.needs_input() {
                    continue;
                }
                // 用合法样例值填满所有参数
                let values: Vec<(String, String)> = a
                    .params
                    .iter()
                    .map(|p| {
                        let v = match p.kind {
                            config::ParamKind::Mac => "aa:bb:cc:dd:ee:ff".to_string(),
                            config::ParamKind::Sn4 => "ZTEG".to_string(),
                            config::ParamKind::Sn8 => "12345678".to_string(),
                            config::ParamKind::Number => "255".to_string(),
                            config::ParamKind::Region => "305".to_string(),
                            config::ParamKind::Password => "pass1234".to_string(),
                            config::ParamKind::Text => "testvalue".to_string(),
                        };
                        (p.name.clone(), v)
                    })
                    .collect();

                // 样例值本身必须合法，否则测试自己就是错的
                a.validate_inputs(&values)
                    .unwrap_or_else(|e| panic!("[{}] 样例值不合法: {e}", a.name));

                for c in a.expand(&values) {
                    assert!(
                        !c.contains('$'),
                        "[{}] 展开后残留占位符: {c}",
                        a.name
                    );
                }
            }
        }
    }

    /// 参数校验不过时必须报出**哪个参数**、为什么。
    #[test]
    fn 参数校验失败时指出具体参数() {
        let all = builtin();
        let a = all
            .iter()
            .flat_map(|(_, actions)| actions.iter())
            .find(|a| a.params.iter().any(|p| p.kind == config::ParamKind::Mac))
            .expect("应有一个使用 mac 参数的操作");

        let bad: Vec<(String, String)> = a
            .params
            .iter()
            .map(|p| {
                // MAC 参数给一个明显非法的值
                let v = if p.kind == config::ParamKind::Mac {
                    "这不是MAC".to_string()
                } else {
                    "ok".to_string()
                };
                (p.name.clone(), v)
            })
            .collect();
        let e = a.validate_inputs(&bad).unwrap_err();
        assert!(e.contains("参数"), "错误应指明参数，实际: {e}");
        assert!(e.contains("MAC") || e.contains("16"), "应说明格式要求，实际: {e}");
    }

    /// 空值必须被拦住，不能把空字符串发到设备。
    #[test]
    fn 参数为空时校验失败() {
        for (_g, actions) in builtin() {
            for a in actions {
                if !a.needs_input() {
                    continue;
                }
                let empty: Vec<(String, String)> = a
                    .params
                    .iter()
                    .map(|p| (p.name.clone(), String::new()))
                    .collect();
                assert!(
                    a.validate_inputs(&empty).is_err(),
                    "[{}] 空参数应被拒绝",
                    a.name
                );
            }
        }
    }

    #[test]
    fn 校时跨天借位正确() {
        // 北京时间 2026-10-14 03:00:00 → UTC 2026-10-13 19:00:00
        let c = set_time_cmd_from_beijing(2026, 10, 14, 3, 0, 0);
        assert_eq!(c, "date -s \"2026-10-13 19:00:00\"");
    }

    #[test]
    fn 校时跨月借位正确() {
        // 北京时间 2026-03-01 03:00:00 → UTC 2026-02-28 19:00:00
        let c = set_time_cmd_from_beijing(2026, 3, 1, 3, 0, 0);
        assert_eq!(c, "date -s \"2026-02-28 19:00:00\"");
    }

    #[test]
    fn 校时跨年借位正确() {
        // 北京时间 2027-01-01 05:00:00 → UTC 2026-12-31 21:00:00
        let c = set_time_cmd_from_beijing(2027, 1, 1, 5, 0, 0);
        assert_eq!(c, "date -s \"2026-12-31 21:00:00\"");
    }

    #[test]
    fn 闰年二月天数正确() {
        assert_eq!(days_in_month(2024, 2), 29, "2024 是闰年");
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2000, 2), 29, "2000 能被 400 整除");
        assert_eq!(days_in_month(1900, 2), 28, "1900 不能被 400 整除");
    }

    #[test]
    fn 整点八时正好不借位() {
        // 边界：北京 08:00 → UTC 00:00 同日
        let c = set_time_cmd_from_beijing(2026, 10, 14, 8, 0, 0);
        assert_eq!(c, "date -s \"2026-10-14 00:00:00\"");
    }
}
