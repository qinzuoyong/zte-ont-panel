//! 连接参数的记忆与恢复。
//!
//! ## 安全约束（重要）
//!
//! 按项目的凭据红线：**密码不落盘**。
//! 这里只记住地址 / 端口 / 账号三项，密码每次运行都要重新输入。
//!
//! 这不是偷懒 —— 光猫的 Telnet 密码往往是公网可检索的弱口令（如 `Zte521`），
//! 把它明文写在 exe 旁边的文本文件里，等于把「谁能物理接触这台电脑」
//! 变成「谁能登录这台光猫」。多打一次密码的成本远低于这个风险。
//!
//! ## 行为
//!
//! - 连接成功后**自动记住**当前参数（不需要用户手动点保存）
//! - 下次启动自动回填
//! - 文件损坏或字段缺失时回落到默认值，不报错打断启动

use std::path::{Path, PathBuf};

/// 被记住的连接参数（不含密码）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionSettings {
    pub host: String,
    pub port: String,
    pub user: String,
}

impl Default for ConnectionSettings {
    fn default() -> Self {
        // 与 zteOnu 固化后的常见默认值一致
        Self {
            host: "192.168.1.1".into(),
            port: "23".into(),
            user: "root".into(),
        }
    }
}

/// 设置文件名。
pub const SETTINGS_FILENAME: &str = "连接设置.txt";

impl ConnectionSettings {
    /// 渲染成可编辑的纯文本。
    ///
    /// 用纯文本而不是二进制：用户想改默认地址时能直接编辑。
    /// 注意这里**没有密码字段**，不要加。
    pub fn render(&self) -> String {
        format!(
            "# 中兴光猫控制面板 —— 连接设置\n\
             # 这个文件由程序在连接成功后自动更新，也可以手动编辑。\n\
             # 出于安全考虑，密码不会保存在这里，每次运行需要重新输入。\n\
             \n\
             address={}\n\
             port={}\n\
             user={}\n",
            self.host, self.port, self.user
        )
    }

    /// 解析设置文本。缺失字段沿用 `default` 的值，不报错。
    ///
    /// 刻意「宽容解析」：这个文件是辅助功能，
    /// 因为一行写错就让用户连不上设备，代价远大于收益。
    pub fn parse(text: &str, default: &Self) -> Self {
        let mut out = default.clone();
        // 去掉 UTF-8 BOM：用户在记事本里改这个文件时很容易带上，
        // BOM（U+FEFF）不是空白字符，trim() 去不掉，会导致
        // 第一行 "address=..." 匹配不上而静默回落到默认地址。
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim().to_string();
            match k.trim() {
                // 空值视为未设置，沿用默认 —— 否则会回填一个空地址
                "address" if !v.is_empty() => out.host = v,
                "port" if !v.is_empty() => out.port = v,
                "user" if !v.is_empty() => out.user = v,
                _ => {}
            }
        }
        out
    }
}

/// 设置文件位置：`数据/连接设置.txt`。
pub fn settings_path() -> Option<PathBuf> {
    crate::paths::settings_file()
}

/// 从指定路径加载。文件不存在或读不了时返回默认值。
pub fn load_from(path: &Path) -> ConnectionSettings {
    let default = ConnectionSettings::default();
    match std::fs::read_to_string(path) {
        Ok(text) => ConnectionSettings::parse(&text, &default),
        Err(_) => default,
    }
}

/// 按标准位置加载。
pub fn load() -> ConnectionSettings {
    match settings_path() {
        Some(p) => load_from(&p),
        None => ConnectionSettings::default(),
    }
}

/// 保存到指定路径。
///
/// 写失败不算致命：记不住参数只是下次要多填一遍，
/// 不该因此打断用户正在做的事。
pub fn save_to(path: &Path, s: &ConnectionSettings) -> std::io::Result<()> {
    // 原子写：被强杀时不留下半截文件（见 paths::write_atomic）
    crate::paths::write_atomic(path, s.render().as_bytes())
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    /// 带 UTF-8 BOM 的连接设置必须能正确读出，不能静默回落默认值。
    #[test]
    fn 带BOM的连接设置能正确读出() {
        let def = ConnectionSettings::default();
        let text = "\u{feff}address=10.0.0.9\nport=2323\nuser=admin\n";
        let s = ConnectionSettings::parse(text, &def);
        assert_eq!(s.host, "10.0.0.9", "BOM 让 address 行失效了");
        assert_eq!(s.port, "2323");
        assert_eq!(s.user, "admin");
        // 与不带 BOM 的结果一致
        let plain = "address=10.0.0.9\nport=2323\nuser=admin\n";
        assert_eq!(s, ConnectionSettings::parse(plain, &def));
    }

    use super::*;

    #[test]
    fn 默认值合理() {
        let d = ConnectionSettings::default();
        assert_eq!(d.host, "192.168.1.1");
        assert_eq!(d.port, "23");
        assert_eq!(d.user, "root");
    }

    /// **安全红线**：渲染结果里绝不能出现密码字段。
    #[test]
    fn 渲染结果不含密码() {
        let s = ConnectionSettings::default();
        let text = s.render();
        assert!(
            !text.to_lowercase().contains("pass"),
            "设置文件不该包含密码字段:\n{text}"
        );
        assert!(
            !text.to_lowercase().contains("pwd"),
            "设置文件不该包含密码字段:\n{text}"
        );
        assert!(!text.contains("Zte521"), "不该出现任何默认口令");
    }

    #[test]
    fn 渲染与解析可往返() {
        let s = ConnectionSettings {
            host: "10.0.0.1".into(),
            port: "2323".into(),
            user: "admin".into(),
        };
        let back = ConnectionSettings::parse(&s.render(), &ConnectionSettings::default());
        assert_eq!(back, s);
    }

    #[test]
    fn 缺失字段沿用默认() {
        let d = ConnectionSettings::default();
        let parsed = ConnectionSettings::parse("address=10.0.0.1\n", &d);
        assert_eq!(parsed.host, "10.0.0.1");
        // 其余字段保持默认
        assert_eq!(parsed.port, d.port);
        assert_eq!(parsed.user, d.user);
    }

    #[test]
    fn 空值不覆盖默认() {
        // 写成 address= 时不该回填一个空地址（那会导致连不上且看不出原因）
        let d = ConnectionSettings::default();
        let parsed = ConnectionSettings::parse("address=\nport=\nuser=\n", &d);
        assert_eq!(parsed, d, "空值应沿用默认");
    }

    #[test]
    fn 注释与空行被忽略() {
        let d = ConnectionSettings::default();
        let text = "# 注释\n\naddress=1.2.3.4\n\n# 又一行注释\nport=23\n";
        let parsed = ConnectionSettings::parse(text, &d);
        assert_eq!(parsed.host, "1.2.3.4");
        assert_eq!(parsed.port, "23");
    }

    #[test]
    fn 坏行不影响其他行() {
        // 没有等号的行、未知键都跳过，不报错
        let d = ConnectionSettings::default();
        let text = "这行没有等号\nbogus=value\naddress=5.6.7.8\n";
        let parsed = ConnectionSettings::parse(text, &d);
        assert_eq!(parsed.host, "5.6.7.8");
    }

    #[test]
    fn 值里含等号时只切第一个() {
        // 用 split_once，不是 split
        let d = ConnectionSettings::default();
        let parsed = ConnectionSettings::parse("address=a=b\n", &d);
        assert_eq!(parsed.host, "a=b");
    }

    #[test]
    fn 值两侧空白被裁掉() {
        let d = ConnectionSettings::default();
        let parsed = ConnectionSettings::parse("address=  1.1.1.1  \n", &d);
        assert_eq!(parsed.host, "1.1.1.1");
    }

    #[test]
    fn 文件不存在时用默认() {
        let p = Path::new("__definitely_missing_settings__.txt");
        assert_eq!(load_from(p), ConnectionSettings::default());
    }

    #[test]
    fn 写盘后可读回() {
        let dir = std::env::temp_dir();
        let p = dir.join("zte_settings_roundtrip.txt");
        let s = ConnectionSettings {
            host: "192.168.2.1".into(),
            port: "23".into(),
            user: "root".into(),
        };
        save_to(&p, &s).expect("写盘失败");
        assert_eq!(load_from(&p), s);
        // 落盘内容同样不含密码字段
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(!text.to_lowercase().contains("pass"));
        let _ = std::fs::remove_file(&p);
    }
}
