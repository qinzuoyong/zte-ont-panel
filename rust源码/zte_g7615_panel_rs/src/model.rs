//! 机型识别：连上设备后自动判断是哪一款中兴光猫，据此选择命令清单。
//!
//! ## 为什么需要这个
//!
//! 中兴光猫的表名、字段名、可用命令在不同机型间差异很大。实测两个例子：
//!
//! | 命令 | G7615（2023 万兆） | F650（2017 GPON） |
//! |---|---|---|
//! | `upgradetest getver` | 返回版本号 | `function not found!!!` |
//! | `cat /proc/capability/boardinfo` | 返回硬件信息 | 文件不存在 |
//! | `sendcmd 1 DB p WLANPSK` | 表不存在 | 返回 4 行 PSK |
//! | `sendcmd 1 DB p WLANCfg` | 加密字段新命名 | 用 `11iAuthMode` 老命名 |
//!
//! 如果按单一机型硬编码，换一台设备就有一堆按钮点了报错。
//! 所以连上后先探测机型，再决定界面显示哪些操作。
//!
//! ## 判定依据
//!
//! 只用**只读**的探测命令，不改任何配置：
//! - `cat /proc/cpuinfo` 的 `Hardware:` 行（最可靠，机型专属 SoC）
//! - `sendcmd 1 DB p DevInfo` 的 `ModelName`
//! - 若干特征命令的可用性（`upgradetest getver` 等）

/// 已知的中兴光猫机型族。
///
/// 不确定时用 `Unknown`，此时按「通用最小集」展示 ——
/// 宁可按通用集工作，也不要凭猜测显示会报错的按钮。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Model {
    /// 2023 年万兆光猫族：G7615 / G7615V2 / F7607P / F7015TV3 等
    G7615Series,
    /// 2017 年前后的 GPON 光猫：F650 / F660 / F670 等
    F650Series,
    /// 没认出来。按通用最小集工作。
    Unknown,
}

impl Model {
    /// 显示名，用于界面提示。
    pub fn display_name(self) -> &'static str {
        match self {
            Model::G7615Series => "G7615 系列（万兆）",
            Model::F650Series => "F650 系列（GPON）",
            Model::Unknown => "未知机型",
        }
    }

    /// 该机型对应的预设清单文件名（不含前缀）。
    ///
    /// 加载顺序：`命令预设_<机型>.txt` → `命令预设.txt` → 内置清单。
    pub fn preset_suffix(self) -> &'static str {
        match self {
            Model::G7615Series => "g7615",
            Model::F650Series => "f650",
            Model::Unknown => "",
        }
    }

    /// 该机型上**已知不可用**的命令片段。
    ///
    /// 用来在加载预设后把会报错的命令剔掉。这不是洁癖 ——
    /// 点了报错的按钮会让用户怀疑是程序坏了，而不是机型不支持。
    pub fn unsupported_fragments(self) -> &'static [&'static str] {
        match self {
            Model::G7615Series => &[],
            Model::F650Series => &[
                // F650 上实测：这两个都不可用
                "upgradetest getver",
                "/proc/capability/boardinfo",
                // G7615 才有 10G 双模切换
                "setmac 3 40962",
            ],
            // 未知机型：只保留最保守的命令
            Model::Unknown => &["upgradetest getver", "/proc/capability/boardinfo"],
        }
    }
}

/// 一次探测的原始证据，便于排查「为什么认成了这个机型」。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeEvidence {
    /// `cat /proc/cpuinfo` 里的 Hardware 行
    pub hardware: Option<String>,
    /// `DevInfo` 里的 ModelName
    pub model_name: Option<String>,
    /// `DevInfo` 里的 SoftwareVer
    pub software_ver: Option<String>,
}

impl ProbeEvidence {
    /// 根据证据判定机型。
    ///
    /// 判定顺序（从可靠到不可靠）：
    /// 1. CPU 的 Hardware 行 —— SoC 型号与机型强绑定
    /// 2. ModelName 前缀 —— 有时是通用名（如 `e8C 4+1 AP`），不如 SoC 可靠
    /// 3. 版本号年份 —— 兜底线索
    pub fn detect(&self) -> Model {
        // 1) SoC
        if let Some(hw) = &self.hardware {
            let h = hw.to_lowercase();
            // G7615/F7607P 等万兆机型用 ZX279132 / ZX279133
            if h.contains("zx279132") || h.contains("zx279133") {
                return Model::G7615Series;
            }
            // F650/F660 等 GPON 机型用 ZX279127
            if h.contains("zx279127") {
                return Model::F650Series;
            }
        }

        // 2) ModelName
        if let Some(m) = &self.model_name {
            let m = m.to_uppercase();
            if m.contains("G7615") || m.contains("F7607") || m.contains("F7015")
                || m.contains("F7005") || m.contains("F7615")
            {
                return Model::G7615Series;
            }
            if m.contains("F650") || m.contains("F660") || m.contains("F670")
                || m.contains("F673")
            {
                return Model::F650Series;
            }
        }

        // 3) 版本年份兜底：2017 年前后的是老机型
        if let Some(v) = &self.software_ver {
            if v.contains("V2.0.0P1T1") || v.contains("V2.0.0T14") {
                return Model::F650Series;
            }
        }

        Model::Unknown
    }
}

/// 从命令输出里抽取探测证据。
///
/// 输入是 `(命令, 输出)` 列表。找不到的项留 None。
pub fn extract_evidence(outputs: &[(String, String)]) -> ProbeEvidence {
    let mut ev = ProbeEvidence::default();

    for (cmd, out) in outputs {
        if cmd.contains("/proc/cpuinfo") {
            // 形如 `Hardware        : ZTE ZX279127 (Device Tree)`
            for line in out.lines() {
                if let Some((k, v)) = line.split_once(':') {
                    if k.trim().eq_ignore_ascii_case("Hardware") {
                        ev.hardware = Some(v.trim().to_string());
                    }
                }
            }
        }
        if cmd.contains("DevInfo") {
            for line in out.lines() {
                let line = line.trim();
                if let Some(v) = attr_value(line, "ModelName") {
                    ev.model_name = Some(v);
                }
                if let Some(v) = attr_value(line, "SoftwareVer") {
                    ev.software_ver = Some(v);
                }
            }
        }
    }
    ev
}

/// 从 `DM name="X" val="Y"` 里取 X=Y 的值。
fn attr_value(line: &str, name: &str) -> Option<String> {
    let pat = format!("name=\"{name}\"");
    if !line.contains(&pat) {
        return None;
    }
    let i = line.find("val=\"")? + 5;
    let j = line[i..].find('"')? + i;
    Some(line[i..j].to_string())
}

/// 探测机型需要跑的命令（全部只读）。
pub fn probe_commands() -> &'static [&'static str] {
    &[
        "cat /proc/cpuinfo",
        "sendcmd 1 DB p DevInfo",
    ]
}

/// 按机型过滤命令：把该机型已知不支持的命令剔掉，返回保留的命令。
///
/// 返回 `(保留的命令, 被剔除的命令)`，后者用于日志提示 ——
/// 用户应当知道「为什么这个操作在这里少了条命令」。
pub fn filter_commands(model: Model, cmds: &[String]) -> (Vec<String>, Vec<String>) {
    let bad = model.unsupported_fragments();
    let mut kept = Vec::new();
    let mut removed = Vec::new();
    for c in cmds {
        if bad.iter().any(|f| c.contains(f)) {
            removed.push(c.clone());
        } else {
            kept.push(c.clone());
        }
    }
    (kept, removed)
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    /// 实测的 F650 输出。
    const F650_CPUINFO: &str = "processor       : 0\nmodel name      : ARMv7 Processor rev 1 (v7l)\nHardware        : ZTE ZX279127 (Device Tree)\nRevision        : 0000";
    const F650_DEVINFO: &str = "<Tbl name=\"DevInfo\" RowCount=\"1\">\n<Row No=\"0\">\n<DM name=\"ModelName\" val=\"ZXHN F650\"/>\n<DM name=\"SoftwareVer\" val=\"V2.0.0P1T1\"/>\n</Row>\n</Tbl>";

    fn f650_outputs() -> Vec<(String, String)> {
        vec![
            ("cat /proc/cpuinfo".into(), F650_CPUINFO.into()),
            ("sendcmd 1 DB p DevInfo".into(), F650_DEVINFO.into()),
        ]
    }

    #[test]
    fn 从真实F650输出识别出机型() {
        let ev = extract_evidence(&f650_outputs());
        assert_eq!(ev.hardware.as_deref(), Some("ZTE ZX279127 (Device Tree)"));
        assert_eq!(ev.model_name.as_deref(), Some("ZXHN F650"));
        assert_eq!(ev.software_ver.as_deref(), Some("V2.0.0P1T1"));
        assert_eq!(ev.detect(), Model::F650Series);
    }

    #[test]
    fn 万兆机型的SoC被识别() {
        let ev = ProbeEvidence {
            hardware: Some("ZTE ZX279132 (Device Tree)".into()),
            ..Default::default()
        };
        assert_eq!(ev.detect(), Model::G7615Series);

        let ev2 = ProbeEvidence {
            hardware: Some("ZTE ZX279133".into()),
            ..Default::default()
        };
        assert_eq!(ev2.detect(), Model::G7615Series);
    }

    #[test]
    fn 只有型号名时也能识别() {
        let ev = ProbeEvidence {
            model_name: Some("ZXHN G7615".into()),
            ..Default::default()
        };
        assert_eq!(ev.detect(), Model::G7615Series);

        let ev2 = ProbeEvidence {
            model_name: Some("ZXHN F650".into()),
            ..Default::default()
        };
        assert_eq!(ev2.detect(), Model::F650Series);
    }

    /// SoC 比 ModelName 更可靠：两者冲突时以 SoC 为准。
    #[test]
    fn SoC优先于型号名() {
        let ev = ProbeEvidence {
            hardware: Some("ZTE ZX279127".into()),
            model_name: Some("ZXHN G7615".into()), // 矛盾的数据
            ..Default::default()
        };
        assert_eq!(ev.detect(), Model::F650Series, "应以 SoC 为准");
    }

    #[test]
    fn 认不出来时返回Unknown() {
        let ev = ProbeEvidence {
            hardware: Some("Some Other SoC".into()),
            model_name: Some("未知设备".into()),
            ..Default::default()
        };
        assert_eq!(ev.detect(), Model::Unknown);
    }

    #[test]
    fn 空证据返回Unknown() {
        assert_eq!(ProbeEvidence::default().detect(), Model::Unknown);
    }

    /// **关键**：F650 上已知不可用的命令必须被剔除。
    #[test]
    fn F650剔除已知不可用命令() {
        let cmds = vec![
            "upgradetest getver".to_string(),
            "cat /proc/capability/boardinfo".to_string(),
            "uptime".to_string(),
            "sendcmd 1 DB p DevInfo".to_string(),
        ];
        let (kept, removed) = filter_commands(Model::F650Series, &cmds);
        assert_eq!(kept, vec!["uptime", "sendcmd 1 DB p DevInfo"]);
        assert_eq!(removed.len(), 2, "应剔除两条: {removed:?}");
        assert!(removed.iter().any(|c| c.contains("getver")));
        assert!(removed.iter().any(|c| c.contains("boardinfo")));
    }

    #[test]
    fn G7615不剔除任何命令() {
        let cmds = vec![
            "upgradetest getver".to_string(),
            "cat /proc/capability/boardinfo".to_string(),
        ];
        let (kept, removed) = filter_commands(Model::G7615Series, &cmds);
        assert_eq!(kept.len(), 2);
        assert!(removed.is_empty());
    }

    /// 未知机型也要剔除最不保险的命令，避免点了报错。
    #[test]
    fn 未知机型保守剔除() {
        let cmds = vec![
            "upgradetest getver".to_string(),
            "uptime".to_string(),
        ];
        let (kept, removed) = filter_commands(Model::Unknown, &cmds);
        assert_eq!(kept, vec!["uptime"]);
        assert_eq!(removed.len(), 1);
    }

    #[test]
    fn 探测命令都是只读的() {
        for c in probe_commands() {
            assert!(!c.contains(" set "), "探测命令不该写配置: {c}");
            assert!(!c.contains("save"), "探测命令不该保存: {c}");
            assert!(!c.contains("reboot"), "探测命令不该重启: {c}");
        }
    }

    #[test]
    fn 机型显示名与后缀() {
        assert_eq!(Model::F650Series.preset_suffix(), "f650");
        assert_eq!(Model::G7615Series.preset_suffix(), "g7615");
        assert_eq!(Model::Unknown.preset_suffix(), "");
        assert!(Model::F650Series.display_name().contains("F650"));
    }

    #[test]
    fn 属性抽取() {
        assert_eq!(
            attr_value("  <DM name=\"ModelName\" val=\"ZXHN F650\"/>", "ModelName"),
            Some("ZXHN F650".to_string())
        );
        assert_eq!(attr_value("<DM name=\"Other\" val=\"x\"/>", "ModelName"), None);
    }

    /// 缺 CPU 信息时不该 panic，应优雅退到 ModelName。
    #[test]
    fn 缺少cpuinfo不崩() {
        let outputs = vec![("sendcmd 1 DB p DevInfo".to_string(), F650_DEVINFO.to_string())];
        let ev = extract_evidence(&outputs);
        assert!(ev.hardware.is_none());
        assert_eq!(ev.detect(), Model::F650Series, "应靠 ModelName 识别");
    }
}
