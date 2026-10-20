//! 外观（明暗主题）偏好的记忆与恢复。
//!
//! ## 为什么单独一个文件，而不塞进 `连接设置.txt`
//!
//! 连接设置是「连哪台设备」，外观是「界面长什么样」。
//! 混在一个文件里，用户想改主题就得翻到一堆地址端口中间去找，
//! 而且两者的写入时机也不同（连接设置是连接成功才写，外观是点一下立即写）。
//!
//! ## 格式
//!
//! 与 `settings.rs` 一致：纯文本、`key=value`、`#` 开头是注释。
//! 只有一个键：
//!
//! ```text
//! theme=light
//! ```
//!
//! 合法值只有 `light` / `dark` / `system` 三个。
//!
//! ## 容错
//!
//! 文件缺失、损坏、值不认 —— 一律回落到 `System`（默认值）。
//! 外观是纯锦上添花的东西，不该因为一行写错就让程序起不来。
//! 这条「一律回落默认值」也是验收要求（默认跟随系统）。

use std::path::{Path, PathBuf};

/// 用户的外观选择。
///
/// 与 `egui::ThemePreference` 一一对应，但**不直接复用**它：
/// 那是个 UI 库的类型，把它写进磁盘格式意味着以后换 UI 库要改存档。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThemeChoice {
    /// 浅色
    Light,
    /// 深色
    Dark,
    /// 跟随系统（默认）
    ///
    /// 默认值取它而不是浅色：Windows/macOS 的浅深偏好是用户早已设好的系统级选择，
    /// 程序自己再猜一次「你要浅色」只会让夜间开窗口白得刺眼。
    #[default]
    System,
}

impl ThemeChoice {
    /// 写进文件的字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            ThemeChoice::Light => "light",
            ThemeChoice::Dark => "dark",
            ThemeChoice::System => "system",
        }
    }

    /// 从文件内容解析。不认识的写法返回 `None`，由调用方回落默认。
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "light" => Some(ThemeChoice::Light),
            "dark" => Some(ThemeChoice::Dark),
            "system" => Some(ThemeChoice::System),
            _ => None,
        }
    }

    /// 界面上显示的中文名。
    pub fn label(self) -> &'static str {
        match self {
            ThemeChoice::Light => "浅色",
            ThemeChoice::Dark => "深色",
            ThemeChoice::System => "跟随系统",
        }
    }
}

impl From<ThemeChoice> for eframe::egui::ThemePreference {
    fn from(c: ThemeChoice) -> Self {
        match c {
            ThemeChoice::Light => eframe::egui::ThemePreference::Light,
            ThemeChoice::Dark => eframe::egui::ThemePreference::Dark,
            ThemeChoice::System => eframe::egui::ThemePreference::System,
        }
    }
}

impl From<eframe::egui::ThemePreference> for ThemeChoice {
    fn from(p: eframe::egui::ThemePreference) -> Self {
        match p {
            eframe::egui::ThemePreference::Light => ThemeChoice::Light,
            eframe::egui::ThemePreference::Dark => ThemeChoice::Dark,
            eframe::egui::ThemePreference::System => ThemeChoice::System,
        }
    }
}

/// 设置文件名。
pub const APPEARANCE_FILENAME: &str = "外观设置.txt";

/// 渲染成可编辑的纯文本。
///
/// 注意这里**没有也不该有任何凭据字段** —— 与 `settings.rs` 同一条红线。
pub fn render(choice: ThemeChoice) -> String {
    format!(
        "# 中兴光猫控制面板 —— 外观设置\n\
         # 这个文件由程序在切换主题时自动更新，也可以手动编辑。\n\
         # theme 只能是 light / dark / system 三者之一。\n\
         # 写错或删掉这个文件都会回落到跟随系统。\n\
         \n\
         theme={}\n",
        choice.as_str()
    )
}

/// 解析设置文本。缺失、空值、不认识的值一律回落默认值（跟随系统）。
pub fn parse(text: &str) -> ThemeChoice {
    // 去掉 UTF-8 BOM：记事本存「UTF-8 with BOM」时 BOM 会粘在第一行，
    // 而 BOM 不是空白字符，trim() 去不掉，会让 theme=... 整行认不出来。
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == "theme" {
            if let Some(c) = ThemeChoice::from_str(v) {
                return c;
            }
        }
    }
    ThemeChoice::default()
}

/// 设置文件位置：`数据/外观设置.txt`。
pub fn appearance_path() -> Option<PathBuf> {
    crate::paths::data_dir().map(|d| d.join(APPEARANCE_FILENAME))
}

/// 从指定路径加载。文件不存在或读不了时返回默认（跟随系统）。
pub fn load_from(path: &Path) -> ThemeChoice {
    match std::fs::read_to_string(path) {
        Ok(text) => parse(&text),
        Err(_) => ThemeChoice::default(),
    }
}

/// 按标准位置加载。
pub fn load() -> ThemeChoice {
    match appearance_path() {
        Some(p) => load_from(&p),
        None => ThemeChoice::default(),
    }
}

/// 保存到指定路径。写失败不算致命 —— 下次启动回到浅色而已。
pub fn save_to(path: &Path, choice: ThemeChoice) -> std::io::Result<()> {
    // 原子写：与连接设置同一套（见 paths::write_atomic）
    crate::paths::write_atomic(path, render(choice).as_bytes())
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    /// 带 UTF-8 BOM 的外观设置必须能读出，否则用户切深色也会被 BOM 打回默认。
    #[test]
    fn 带BOM的外观设置能正确读出() {
        assert_eq!(parse("\u{feff}theme=dark\n"), ThemeChoice::Dark);
        assert_eq!(parse("\u{feff}theme=light\n"), ThemeChoice::Light);
        assert_eq!(parse("\u{feff}theme=system\n"), ThemeChoice::System);
    }

    use super::*;

    /// 验收要求「默认跟随系统」：没有任何配置文件时必须回落到 system。
    #[test]
    fn 默认是跟随系统() {
        assert_eq!(ThemeChoice::default(), ThemeChoice::System);
        assert_eq!(
            load_from(Path::new("__definitely_missing_appearance__.txt")),
            ThemeChoice::System
        );
        // 反过来锁死：默认值不许偷偷变回浅色
        assert_ne!(ThemeChoice::default(), ThemeChoice::Light);
    }

    #[test]
    fn 渲染与解析可往返() {
        for c in [ThemeChoice::Light, ThemeChoice::Dark, ThemeChoice::System] {
            assert_eq!(parse(&render(c)), c, "{:?} 往返失败", c);
        }
    }

    /// 序列化关键字只允许 light / dark / system 三个。
    #[test]
    fn 关键字只有三个且可反解() {
        assert_eq!(ThemeChoice::Light.as_str(), "light");
        assert_eq!(ThemeChoice::Dark.as_str(), "dark");
        assert_eq!(ThemeChoice::System.as_str(), "system");
        for c in [ThemeChoice::Light, ThemeChoice::Dark, ThemeChoice::System] {
            assert_eq!(ThemeChoice::from_str(c.as_str()), Some(c));
        }
        assert_eq!(ThemeChoice::from_str("blue"), None);
        assert_eq!(ThemeChoice::from_str(""), None);
    }

    /// 坏文件一律回落默认值（跟随系统），不许 panic、不许猜。
    #[test]
    fn 坏文件回落跟随系统() {
        assert_eq!(parse(""), ThemeChoice::System);
        assert_eq!(parse("theme=\n"), ThemeChoice::System);
        assert_eq!(parse("theme=紫色\n"), ThemeChoice::System);
        assert_eq!(parse("这行没有等号\ntheme=dark\n"), ThemeChoice::Dark);
        assert_eq!(parse("# theme=dark\n"), ThemeChoice::System, "注释不算");
    }

    #[test]
    fn 大小写与空白被容忍() {
        assert_eq!(parse("theme = DARK \n"), ThemeChoice::Dark);
        assert_eq!(parse("  theme=System  \n"), ThemeChoice::System);
    }

    /// 与 settings.rs 同一条红线：外观文件里不能出现任何凭据字样。
    #[test]
    fn 外观文件不含任何凭据字段() {
        let text = render(ThemeChoice::Dark).to_lowercase();
        for bad in ["pass", "pwd", "user", "host", "port", "address"] {
            assert!(!text.contains(bad), "外观文件不该包含 {bad}");
        }
    }

    #[test]
    fn 写盘后可读回() {
        let dir = std::env::temp_dir();
        let p = dir.join("zte_appearance_roundtrip.txt");
        save_to(&p, ThemeChoice::Dark).expect("写盘失败");
        assert_eq!(load_from(&p), ThemeChoice::Dark);
        let _ = std::fs::remove_file(&p);
    }
}