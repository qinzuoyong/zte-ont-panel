//! 配置备份：写操作前存档原值，支持事后查证与回退。
//!
//! ## 解决的问题
//!
//! 光猫的 `sendcmd 1 DB set` 是**立即生效且没有撤销键**的。
//! 改错一个字段，在界面上看不出任何异常，而设备行为可能已经变了。
//! README 里已记录的典型场景：恢复出厂 / 改区域码会**丢失 Telnet 权限**，
//! 那时如果没有事先记下原值，就只能靠装维上门。
//!
//! 所以任何写操作之前，先把目标表的**原值**抓下来存盘。
//!
//! ## 为什么存成纯文本而不是序列化格式
//!
//! 备份文件的价值在于「设备出问题时还能看懂」。
//! 用 JSON/二进制存，出故障时得先写个解析器才能读；
//! 存成带表头注释的纯文本，记事本能直接看。
//! 而且本机 `--offline` 构建没有 serde_json 可用。

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// 一次备份记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupEntry {
    /// 备份时间（本地北京时间，格式化后的字符串）
    pub timestamp: String,
    /// 触发备份的操作名
    pub action: String,
    /// 备份了哪张表 → 该表的原始回读文本
    pub tables: Vec<(String, String)>,
}

impl BackupEntry {
    /// 渲染成可读文本。
    pub fn render(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "# 备份时间: {}", self.timestamp);
        let _ = writeln!(s, "# 触发操作: {}", self.action);
        let _ = writeln!(s, "# 涉及表数: {}", self.tables.len());
        let _ = writeln!(s, "#");
        let _ = writeln!(s, "# 原值在下方，可用「自定义命令」按需回填。");
        let _ = writeln!(s, "# 注意：回填需要自己把 val=\"...\" 里的值取出来，");
        let _ = writeln!(s, "# 拼成 sendcmd 1 DB set <表> <行> <字段> <值> 再执行。");
        let _ = writeln!(s);
        for (name, text) in &self.tables {
            let _ = writeln!(s, "===== {name} =====");
            let _ = writeln!(s, "{text}");
            let _ = writeln!(s);
        }
        s
    }
}

/// 从一批写命令里推出需要备份哪些表。
///
/// 只认 `sendcmd 1 DB set`；`setmac` 之类的命令不在表里，
/// 由调用方另行处理（目前 `setmac` 走「查看全部系统参数」人工记录）。
pub fn tables_to_backup(cmds: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in cmds {
        if let Some(rb) = crate::dbcli::readback_cmd_for(c) {
            if let Some(t) = rb.strip_prefix("sendcmd 1 DB p ") {
                let t = t.trim().to_string();
                // 同一张表只备份一次
                if !out.contains(&t) {
                    out.push(t);
                }
            }
        }
    }
    out
}

/// 备份目录：`数据/配置备份/`。
///
/// 路径计算统一交给 `paths` 模块，避免各处各算一份、改目录结构时漏改。
pub fn backup_dir() -> Option<PathBuf> {
    crate::paths::backup_dir()
}

/// 把一次备份写盘。返回写入的文件路径。
///
/// 文件名用「时间戳 + 操作名」，操作名里的路径分隔符等字符会被替换掉——
/// 操作名来自预设文件（用户可编辑），不清理可能写出到目录外。
/// 用「写临时文件 → 落盘 → 改名」的方式写，而不是直接 `write`。
///
/// 备份的价值全在「设备出事后还能看懂」。直接 `write` 时，进程在写盘
/// 途中被强杀（用户以为程序卡死点了结束任务、或断电）会留下一个
/// **被截断的文本文件** —— 打开一看像备份，实际后半截没了；
/// 若缺的正是账号表，恢复时会被误导。
///
/// `sync_all` 是必要的：只 `write` 不 `sync` 时数据还在系统缓存里，
/// 掉电后文件可能长度为 0 而文件名照样存在。
pub fn save(dir: &Path, entry: &BackupEntry) -> std::io::Result<PathBuf> {
    use std::io::Write as _;
    std::fs::create_dir_all(dir)?;
    let safe_action = sanitize_filename(&entry.action);
    let stamp = entry.timestamp.replace([':', ' ', '-'], "");
    let name = format!("{}_{}.txt", stamp, safe_action);
    let path = dir.join(&name);
    let tmp = dir.join(format!("{name}.part"));

    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(entry.render().as_bytes())?;
        f.sync_all()?;
    }
    // rename 在同一分区内是原子的：要么看到完整的新文件，要么还是旧的。
    // 失败时清掉临时文件，别在备份目录里留下一堆 .part。
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(path)
}

/// 清理成安全的文件名片段。
///
/// 必须防路径穿越：操作名来自用户可编辑的预设文件，
/// 写成 `../../x` 就能跑到备份目录外面去。
pub fn sanitize_filename(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c {
            // Windows 文件名非法字符 + 路径分隔符
            '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    // 去掉首尾的点与空格：Windows 不允许文件名以点或空格结尾
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        "未命名操作".to_string()
    } else {
        // 限制长度，避免超过文件系统上限
        trimmed.chars().take(60).collect()
    }
}

/// 把时间戳格式化成 `YYYY-MM-DD HH-MM-SS`（本地北京时间）。
///
/// 只用于文件名与备份头，精度到秒足够。
pub fn format_timestamp(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32) -> String {
    format!("{:04}-{:02}-{:02} {:02}-{:02}-{:02}", y, mo, d, h, mi, s)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn 从写命令推出要备份的表() {
        let cmds = vec![
            "sendcmd 1 DB set TelnetCfg 0 TS_Enable 1".to_string(),
            "sendcmd 1 DB set TelnetCfg 0 Wan_Enable 0".to_string(), // 同表，不该重复
            "sendcmd 1 DB set MgtServer 0 Tr069Enable 0".to_string(),
            "sendcmd 1 DB save".to_string(), // 无表
            "reboot".to_string(),            // 无表
        ];
        assert_eq!(tables_to_backup(&cmds), vec!["TelnetCfg", "MgtServer"]);
    }

    #[test]
    fn 没有写命令时不备份任何表() {
        let cmds = vec!["uptime".to_string(), "sendcmd 1 DB p DevInfo".to_string()];
        assert!(tables_to_backup(&cmds).is_empty());
    }

    #[test]
    fn 渲染结果含时间与操作名与表内容() {
        let e = BackupEntry {
            timestamp: "2026-10-14 13-48-57".into(),
            action: "固化 Telnet".into(),
            tables: vec![(
                "TelnetCfg".into(),
                "<TelnetCfg>\n  DM name=\"TS_Enable\" val=\"0\"/>\n".into(),
            )],
        };
        let s = e.render();
        assert!(s.contains("2026-10-14 13-48-57"));
        assert!(s.contains("固化 Telnet"));
        assert!(s.contains("TelnetCfg"));
        assert!(s.contains("TS_Enable"), "原值必须写进去: {s}");
        assert!(s.contains("val=\"0\""), "原值必须写进去");
    }

    /// **关键**：操作名里的路径穿越字符必须被清理掉。
    ///
    /// 操作名来自用户可编辑的预设文件，不清理就能把文件写到备份目录外。
    #[test]
    fn 文件名清理掉路径穿越() {
        let out = sanitize_filename("../../evil");
        // 分隔符被替换，点号在首尾会被 trim 掉
        assert!(!out.contains('/'), "仍含分隔符: {out}");
        assert!(!out.contains('\\'), "仍含分隔符: {out}");
        assert!(!out.starts_with(".."), "仍以 .. 开头: {out}");
        // 有意义的正文部分应当保留，方便日后辨认
        assert!(out.contains("evil"), "正文被误删: {out}");
    }

    #[test]
    fn 文件名不含非法字符与分隔符() {
        for bad in [
            "a/b",
            "a\\b",
            "a:b",
            "a*b",
            "a?b",
            "a\"b",
            "a<b",
            "a>b",
            "a|b",
        ] {
            let out = sanitize_filename(bad);
            assert!(
                !out.contains('/')
                    && !out.contains('\\')
                    && !out.contains(':')
                    && !out.contains('*')
                    && !out.contains('?')
                    && !out.contains('"')
                    && !out.contains('<')
                    && !out.contains('>')
                    && !out.contains('|'),
                "{bad} 清理后仍含非法字符: {out}"
            );
        }
    }

    #[test]
    fn 文件名不以点或空格结尾() {
        // Windows 不允许文件名以点/空格结尾
        assert!(!sanitize_filename("abc.").ends_with('.'));
        assert!(!sanitize_filename("abc ").ends_with(' '));
        assert!(!sanitize_filename("...").ends_with('.'));
    }

    #[test]
    fn 全非法字符时有兜底名() {
        let out = sanitize_filename("///");
        assert!(!out.is_empty());
        assert!(!out.contains('/'));
    }

    #[test]
    fn 空名有兜底() {
        assert_eq!(sanitize_filename(""), "未命名操作");
        assert_eq!(sanitize_filename("   "), "未命名操作");
    }

    #[test]
    fn 超长名被截断() {
        let long = "长".repeat(200);
        let out = sanitize_filename(&long);
        assert!(out.chars().count() <= 60, "实际 {} 字", out.chars().count());
    }

    #[test]
    fn 写盘后内容可读回() {
        let dir = std::env::temp_dir().join("zte_backup_test");
        let _ = std::fs::remove_dir_all(&dir);
        let e = BackupEntry {
            timestamp: "2026-10-14 13-48-57".into(),
            action: "测试操作".into(),
            tables: vec![("T".into(), "<T>\n  DM name=\"A\" val=\"1\"/>\n".into())],
        };
        let path = save(&dir, &e).expect("写盘失败");
        assert!(path.exists());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("测试操作"));
        assert!(text.contains("val=\"1\""));
        // 文件名应含时间戳
        let fname = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(fname.contains("20261014"), "文件名应含时间戳: {fname}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 恶意操作名不能写到备份目录之外。
    #[test]
    fn 恶意操作名不会写出目录外() {
        let dir = std::env::temp_dir().join("zte_backup_escape_test");
        let _ = std::fs::remove_dir_all(&dir);
        let e = BackupEntry {
            timestamp: "2026-10-14 13-48-57".into(),
            action: "../../../escaped".into(),
            tables: vec![("T".into(), "x".into())],
        };
        let path = save(&dir, &e).expect("写盘失败");
        // 父目录必须仍是备份目录
        assert_eq!(
            path.parent().unwrap(),
            dir.as_path(),
            "文件被写到目录外: {}",
            path.display()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 写盘不能经过「可见的半成品文件」。
    ///
    /// 直接 `write` 时，进程在写盘途中被强杀会留下一个被截断的 .txt ——
    /// 打开一看像备份，实际后半截没了。这里验证两件事：
    /// 写完目录里没有残留的 .part，且内容完整可读。
    #[test]
    fn 备份写完不留临时文件() {
        let dir = std::env::temp_dir().join("zte_backup_atomic_test");
        let _ = std::fs::remove_dir_all(&dir);
        let body = "x".repeat(200_000);
        let e = BackupEntry {
            timestamp: "2026-10-14 13-48-57".into(),
            action: "原子写".into(),
            tables: vec![("T".into(), body.clone())],
        };
        let path = save(&dir, &e).expect("写盘失败");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|x| x.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty(), "不该留下临时文件: {leftovers:?}");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(&body), "内容被截断了");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 时间戳格式固定() {
        assert_eq!(
            format_timestamp(2026, 10, 14, 13, 48, 57),
            "2026-10-14 13-48-57"
        );
        // 单位数补零
        assert_eq!(
            format_timestamp(2026, 1, 2, 3, 4, 5),
            "2026-01-02 03-04-05"
        );
    }
}