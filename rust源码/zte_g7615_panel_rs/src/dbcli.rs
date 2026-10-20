//! 中兴 `sendcmd 1 DB` 表输出的解析、写后回读校验、以及表结构自省。
//!
//! ## 为什么需要这个模块
//!
//! 中兴的 `sendcmd 1 DB set` 在**字段名或行号写错时不会报错**：
//! 命令照样返回、退出码照样是 0、设备什么都不打印。
//! 界面上看到的是「----- 完成 -----」，而配置根本没写进去。
//!
//! 所以所有写操作都必须走「写完立刻回读、比对目标字段」的闭环，
//! 否则用户是在盲操作一台会撒谎的设备。
//!
//! 这个模块只做纯文本解析与命令拼装，**不碰 socket**，
//! 因此可以被单元测试完整覆盖（见文件末尾）。

use std::collections::BTreeMap;

/// 一张 `sendcmd 1 DB p <表名>` 输出里的行。
///
/// 真实输出形如：
/// ```text
/// <DevInfo>
///   <Row No="0">
///     DM name="DeviceType" val="G7615"/>
///     DM name="SerialNumber" val="ZTEG12345678"/>
///   </Row>
/// </DevInfo>
/// ```
/// 注意首行 `<DevInfo>` 以 `>` 结尾，历史上曾被提示符剥离逻辑整行吃掉
/// （见 `telnet.rs` 的 `以尖括号结尾的输出行不能被当成提示符剥掉` 测试）。
/// 这里解析时同样不能把它当成提示符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbRow {
    /// `<Row No="0">` 里的行号
    pub no: String,
    /// 字段名 → 字段值，按出现顺序稳定输出
    pub fields: BTreeMap<String, String>,
}

/// 一张表的完整回读结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbTable {
    /// 表名，取自首行 `<表名>`
    pub name: String,
    pub rows: Vec<DbRow>,
}

impl DbTable {
    /// 取指定行里某个字段的值。
    pub fn get(&self, row_no: &str, field: &str) -> Option<&str> {
        self.rows
            .iter()
            .find(|r| r.no == row_no)
            .and_then(|r| r.fields.get(field))
            .map(|s| s.as_str())
    }

    /// 所有行里第一个叫 `field` 的字段值。用在行号不重要、或行号猜错时的兜底比对。
    pub fn get_any_row(&self, field: &str) -> Option<&str> {
        self.rows.iter().find_map(|r| r.fields.get(field)).map(|s| s.as_str())
    }
}

/// 从一行里抽出 `DM name="X" val="Y"` 这对键值。
///
/// 属性顺序在固件之间可能不同（`val` 在前、`name` 在后都见过），
/// 所以两个属性用独立查找，不假设谁先谁后。
fn parse_dm_line(line: &str) -> Option<(String, String)> {
    // 先剥掉常见的容器前缀。
    //
    // 固件之间的输出形态并不统一：除了文档里那种
    // `<Tbl name="TelnetCfg" RowCount="1">` 之后每行直接以 `DM` 开头，
    // 还有固件把整行写成 `<DM name="X" val="Y"/>`（前置 `<`），
    // 或者先来一层 `<Tbl>` 容器把每个 `DM` 包起来。
    // 只要只认字面量开头 `DM `，这些行会被当成普通正文悄悄丢掉 ——
    // 而丢掉字段的后果是回读校验报「回读表未找到字段」：
    // 一个其实写成功了的操作被判失败，用户会去重试、去怀疑设备。
    let t = line.trim();
    let t = t.strip_prefix('<').unwrap_or(t);
    let t = t.strip_prefix("/").unwrap_or(t);
    if !(t.starts_with("DM ") || t.starts_with("DM\t")) {
        return None;
    }
    let name = extract_attr(t, "name")?;
    // val 允许为空串（清空 SSID 前缀就是写空值），所以这里只要求属性存在
    let val = extract_attr(t, "val").unwrap_or_default();
    Some((name, val))
}

/// 抽 `key="value"` 形式的属性值。属性不存在返回 None。
///
/// 值本身可能含双引号吗？中兴的 `val` 是纯文本配置值，
/// 实践中不含引号；按「到下一个双引号为止」取即可。
fn extract_attr(s: &str, key: &str) -> Option<String> {
    let pat = format!("{}=\"", key);
    let start = s.find(&pat)? + pat.len();
    let end = s[start..].find('"')? + start;
    Some(s[start..end].to_string())
}

/// 解析 `sendcmd 1 DB p <表名>` 的输出。
///
/// 容错要点：
/// - 首行 `<表名>` 可能缺失（设备异常/输出被截断）→ 表名回落为空串，仍尽量解析行
/// - `<Row No="N">` 的引号可能被设备省略 → 用宽松匹配
/// - 没有 `<Row>` 包裹的裸 `DM` 行 → 归入行号 `"0"`，不丢数据
pub fn parse_table(text: &str) -> DbTable {
    let mut name = String::new();
    let mut rows: Vec<DbRow> = Vec::new();
    let mut cur: Option<DbRow> = None;

    for line in text.lines() {
        let t = line.trim();

        // 首行 <表名>：一行就是尖括号包起来的裸标识符
        if name.is_empty() && t.starts_with('<') && t.ends_with('>') {
            let inner = &t[1..t.len() - 1];
            // 排除 <Row ...> / </Row> 这类
            if inner.starts_with("Tbl") {
                // 另一种真实形态：<Tbl name="DevInfo" RowCount="1">。
                // 不认它的话表名会一直空着，界面上显示成 `< >`，
                // 回读结论里的「回读 XX 确认已生效」就变成一个空名字。
                if let Some(n) = extract_attr(inner, "name") {
                    name = n;
                    continue;
                }
            } else if !inner.starts_with("Row") && !inner.starts_with('/') {
                name = inner.to_string();
                continue;
            }
        }

        // <Row No="0"> 开新行
        if let Some(no) = parse_row_open(t) {
            if let Some(r) = cur.take() {
                rows.push(r);
            }
            cur = Some(DbRow { no, fields: BTreeMap::new() });
            continue;
        }

        // </Row> 收口当前行
        if t.starts_with("</Row") {
            if let Some(r) = cur.take() {
                rows.push(r);
            }
            continue;
        }

        // 字段行
        if let Some((k, v)) = parse_dm_line(t) {
            match cur.as_mut() {
                Some(r) => {
                    r.fields.insert(k, v);
                }
                None => {
                    // 没有 <Row> 包裹：建一个行号 0 的匿名行兜住数据
                    let mut r = DbRow { no: "0".into(), fields: BTreeMap::new() };
                    r.fields.insert(k, v);
                    cur = Some(r);
                }
            }
        }
    }

    if let Some(r) = cur.take() {
        rows.push(r);
    }

    DbTable { name, rows }
}

/// 解析 `<Row No="0">`，返回行号。不是 Row 开标签就返回 None。
fn parse_row_open(t: &str) -> Option<String> {
    if !t.starts_with("<Row") || t.starts_with("</Row") {
        return None;
    }
    // 宽松匹配：No="0" / No='0' / No=0 都要认
    let after = &t[4..];
    let idx = after.find("No")? + 4;
    let rest = &t[idx + 2..];
    let rest = rest.trim_start().strip_prefix('=')?.trim_start();
    let rest = rest.trim_start_matches(['"', '\'']);
    let end = rest.find(['"', '\'', '>', ' ', '\t']).unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

/// 从写命令里拆出 (表名, 行号, 字段名, 期望值)。
///
/// 只认 `sendcmd 1 DB set <表名> <行号> <字段> <值>` 这一种形态。
/// 值里可能含空格（比如 `date -s "..."` 不用这条路径，但配置值理论上可以），
/// 所以第 4 段之后的所有内容都算值。
///
/// 不是 set 命令（`save`/`delr`/`p` 等）返回 None —— 那些命令不适用回读校验。
pub fn parse_set_cmd(cmd: &str) -> Option<(String, String, String, String)> {
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    // 合法的 set 命令是 <8 段>：sendcmd 1 DB set <表> <行> <字段> <值...>
    //   toks[0..3] = sendcmd 1 DB     (3)
    //   toks[3]    = set              (1)
    //   toks[4]    = 表名             (1)
    //   toks[5]    = 行号             (1)
    //   toks[6]    = 字段名           (1)
    //   toks[7..]  = 值（至少 1 段）  (>=1)
    // 所以 len < 8 就是「缺值」的不完整命令，必须拒绝。
    // 注意别数成 7：只到字段名为止的命令设备会静默接受但无值可校验。
    if toks.len() < 8 {
        return None;
    }
    if toks[0] != "sendcmd" || toks[1] != "1" || toks[2] != "DB" || toks[3] != "set" {
        return None;
    }
    let table = toks[4].to_string();
    let row = toks[5].to_string();
    let field = toks[6].to_string();
    // 值 = 从第 8 个 token 开始的原文（可能含空格，如 SSID）
    let val = toks[7..].join(" ");
    Some((table, row, field, val))
}

/// 校验结论。
///
/// 分三档而不是简单的 `bool`，因为**「设备把值遮蔽了」和「写入失败」
/// 是完全不同的两件事**，混在一起会让用户以为改坏了。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// 回读确认已生效
    Confirmed,
    /// 写入未生效（值不符 / 字段不存在）
    Failed(String),
    /// 无法判定：设备把该字段遮蔽了（如 WiFi 密码回读为 `******`）。
    ///
    /// 这是实测结论 —— F650 的 `WLANPSK.KeyPassphrase` 无论写入前后
    /// 回读都是 `******`，所以永远无法用回读确认。
    /// 报「失败」会是假警报，报「成功」会是假保证，只能如实说「不知道」。
    Masked,
}

/// 判断一个回读值是否是设备的遮蔽占位符。
///
/// 中兴设备对敏感字段（密码类）回读时用固定长度的星号串遮蔽。
/// 目前实测到的形态是 `******`；这里放宽为「全是 `*` 且长度 >= 3」，
/// 以免不同固件用不同星号个数时漏判。
/// 除半角 `*` 外，部分固件用全角 `＊` 或 `•` 遮蔽。
/// 少认一种的后果是「写入其实成功了，却报 ❌ 值不符」—— 假警报。
pub fn is_masked(value: &str) -> bool {
    let v = value.trim();
    v.chars().count() >= 3
        && v.chars()
            .all(|c| matches!(c, '*' | '＊' | '•' | '●'))
}

/// 判断写命令的目标字段回读后是否等于期望值。
///
/// 比对规则：
/// 1. 优先按「行号 + 字段名」精确比对
/// 2. 行号对不上时，退化为「任意行里找同名字段」——
///    因为设备可能重建行、行号漂移，这时值对了就算生效
/// 3. 值比对忽略首尾空白与大小写差异（设备可能归一化大小写，
///    例如 MAC 存成大写、SSID 前后空格被裁掉）
/// 4. 回读值是遮蔽串（全 `*`）时返回 `Masked`，**不判失败** ——
///    这是设备的保护行为，不是写入出错
pub fn verify_set(cmd: &str, readback: &DbTable) -> Result<(), String> {
    match verdict(cmd, readback) {
        Verdict::Confirmed => Ok(()),
        Verdict::Failed(why) => Err(why),
        // 保持向后兼容：遮蔽不等于失败，返回 Ok 会在 UI 上显示 ✅，
        // 但 UI 用的是 verdict()，所以这里不会误导。
        Verdict::Masked => Ok(()),
    }
}

/// 带三档结论的校验。UI 用这个而不是 `verify_set`，
/// 才能把「无法确认」和「已确认」区分开显示。
pub fn verdict(cmd: &str, readback: &DbTable) -> Verdict {
    let Some((table, row, field, want)) = parse_set_cmd(cmd) else {
        return Verdict::Failed("不是可校验的 set 命令".into());
    };

    let got = readback
        .get(&row, &field)
        .map(|s| s.to_string())
        .or_else(|| readback.get_any_row(&field).map(|s| s.to_string()));

    match got {
        None => Verdict::Failed(format!(
            "回读 {} 表未找到字段 {}（可能字段名不符，或该表无此行号）",
            table, field
        )),
        Some(v) => {
            // 先判遮蔽：设备回读敏感字段时一律给星号串，
            // 此时「值不等」是必然的，不能据此报失败。
            if is_masked(&v) {
                return Verdict::Masked;
            }
            if v.trim().eq_ignore_ascii_case(want.trim()) {
                Verdict::Confirmed
            } else {
                Verdict::Failed(format!(
                    "回读值不符：{} 期望 {:?}，实际 {:?}",
                    field,
                    want.trim(),
                    v.trim()
                ))
            }
        }
    }
}

/// 针对一条 `sendcmd 1 DB set` 生成对应的回读命令。
pub fn readback_cmd_for(cmd: &str) -> Option<String> {
    let (table, _, _, _) = parse_set_cmd(cmd)?;
    Some(format!("sendcmd 1 DB p {}", table))
}

/// 取出一条 `set` 命令操作的字段名，供提示文案使用。
pub fn field_of(cmd: &str) -> Option<String> {
    parse_set_cmd(cmd).map(|(_, _, field, _)| field)
}

/// 把一张表渲染成对齐的文本表格，供 UI 直接显示。
///
/// 相比原来的等宽纯文本 `DM name=... val=...` 堆叠，
/// 表格能让用户一眼看出字段名和值的对应关系。
pub fn render_table(t: &DbTable) -> String {
    if t.rows.is_empty() {
        return format!("<{}>\n(无数据)", t.name);
    }

    // 字段名列宽 = 最长字段名，且给个下限，避免窄表太挤
    let mut field_w = 0usize;
    for r in &t.rows {
        for k in r.fields.keys() {
            field_w = field_w.max(display_width(k));
        }
    }
    field_w = field_w.max(8);

    let mut out = format!("<{}>\n", t.name);
    for r in &t.rows {
        out.push_str(&format!("  Row No={}\n", r.no));
        for (k, v) in &r.fields {
            let pad = field_w.saturating_sub(display_width(k));
            out.push_str(&format!("    {}{}  {}\n", k, " ".repeat(pad), v));
        }
    }
    out.trim_end().to_string()
}

/// 估算字符串的显示宽度：中日韩等全角字符按 2 列算，其余按 1 列。
///
/// 不引入 unicode-width 依赖，因为要覆盖的字符集很窄
/// （表名/字段名都是 ASCII，值里可能有中文 SSID）。
/// 用「非 ASCII 一律按宽字符」的保守估算，够用且零依赖。
fn display_width(s: &str) -> usize {
    s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    /// 真机 `sendcmd 1 DB p DevInfo` 的典型输出形态
    const DEVINFO: &str = "<DevInfo>\r\n  <Row No=\"0\">\r\n    DM name=\"DeviceType\" val=\"G7615\"/>\r\n    DM name=\"SerialNumber\" val=\"ZTEG12345678\"/>\r\n  </Row>\r\n</DevInfo>";

    #[test]
    fn 解析表名与字段() {
        let t = parse_table(DEVINFO);
        assert_eq!(t.name, "DevInfo");
        assert_eq!(t.rows.len(), 1);
        assert_eq!(t.get("0", "DeviceType"), Some("G7615"));
        assert_eq!(t.get("0", "SerialNumber"), Some("ZTEG12345678"));
    }

    #[test]
    fn 首行尖括号不能被当成提示符吃掉() {
        // 回归：解析器不能把 <DevInfo> 这行丢掉
        let t = parse_table("<TelnetCfg>\n  <Row No=\"0\">\n    DM name=\"TS_Enable\" val=\"1\"/>\n  </Row>");
        assert_eq!(t.name, "TelnetCfg", "首行表名丢失");
        assert_eq!(t.get("0", "TS_Enable"), Some("1"));
    }

    #[test]
    fn 多行表全部解析() {
        let text = "<DevAuthInfo>\n  <Row No=\"0\">\n    DM name=\"User\" val=\"telecomadmin\"/>\n  </Row>\n  <Row No=\"1\">\n    DM name=\"User\" val=\"useradmin\"/>\n  </Row>";
        let t = parse_table(text);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.get("0", "User"), Some("telecomadmin"));
        assert_eq!(t.get("1", "User"), Some("useradmin"));
    }

    #[test]
    fn 属性顺序颠倒也能解析() {
        // 有些固件把 val 写在 name 前面
        let t = parse_table("<T>\n  <Row No=\"0\">\n    DM val=\"x\" name=\"F\"/>\n  </Row>");
        assert_eq!(t.get("0", "F"), Some("x"));
    }

    #[test]
    fn 空值字段不被丢弃() {
        // 清空 SSID 前缀就是写空值，必须能解析出 Some("")
        let t = parse_table("<WLANCfg>\n  <Row No=\"0\">\n    DM name=\"ESSIDPrefix\" val=\"\"/>\n  </Row>");
        assert_eq!(t.get("0", "ESSIDPrefix"), Some(""));
    }

    #[test]
    fn 没有Row包裹的裸DM行不丢数据() {
        let t = parse_table("<T>\n  DM name=\"A\" val=\"1\"");
        assert_eq!(t.get("0", "A"), Some("1"));
    }

    #[test]
    fn 无数据表不崩() {
        let t = parse_table("<T>");
        assert_eq!(t.name, "T");
        assert!(t.rows.is_empty());
        assert!(render_table(&t).contains("无数据"));
    }

    #[test]
    fn 解析set命令() {
        let (tbl, row, f, v) = parse_set_cmd("sendcmd 1 DB set TelnetCfg 0 TS_Enable 1").unwrap();
        assert_eq!((tbl.as_str(), row.as_str(), f.as_str(), v.as_str()), ("TelnetCfg", "0", "TS_Enable", "1"));
    }

    #[test]
    fn set命令的值可以含空格() {
        let (_, _, _, v) =
            parse_set_cmd("sendcmd 1 DB set WLANCfg 0 ESSID My Home WiFi").unwrap();
        assert_eq!(v, "My Home WiFi");
    }

    #[test]
    fn 非set命令不参与校验() {
        assert!(parse_set_cmd("sendcmd 1 DB save").is_none());
        assert!(parse_set_cmd("sendcmd 1 DB p DevInfo").is_none());
        assert!(parse_set_cmd("sendcmd 1 DB delr WANC 0").is_none());
        assert!(parse_set_cmd("reboot").is_none());
        assert!(parse_set_cmd("uptime").is_none());
        // 缺值的不完整 set 也不算
        assert!(parse_set_cmd("sendcmd 1 DB set TelnetCfg 0 TS_Enable").is_none());
    }

    #[test]
    fn 回读命令由set命令推导() {
        assert_eq!(
            readback_cmd_for("sendcmd 1 DB set TelnetCfg 0 TS_Enable 1").unwrap(),
            "sendcmd 1 DB p TelnetCfg"
        );
        assert!(readback_cmd_for("sendcmd 1 DB save").is_none());
    }

    #[test]
    fn 校验通过() {
        let t = parse_table("<TelnetCfg>\n  <Row No=\"0\">\n    DM name=\"TS_Enable\" val=\"1\"/>\n  </Row>");
        assert!(verify_set("sendcmd 1 DB set TelnetCfg 0 TS_Enable 1", &t).is_ok());
    }

    #[test]
    fn 值被设备改写时校验失败() {
        // 写进去 1，回读却是 0 —— 必须报错，这正是静默失败的典型形态
        let t = parse_table("<TelnetCfg>\n  <Row No=\"0\">\n    DM name=\"TS_Enable\" val=\"0\"/>\n  </Row>");
        let e = verify_set("sendcmd 1 DB set TelnetCfg 0 TS_Enable 1", &t).unwrap_err();
        assert!(e.contains("回读值不符"), "实际: {e}");
    }

    // ---------- 遮蔽（实测 F650 的 WLANPSK.KeyPassphrase）----------

    /// 实测形态：F650 的 WiFi 密码无论写入前后，回读都是 `******`。
    #[test]
    fn 识别遮蔽串() {
        assert!(is_masked("******"));
        assert!(is_masked("***"));
        assert!(is_masked("  ******  "), "两侧空白应被容忍");
        // 不是遮蔽串的正常值
        assert!(!is_masked(""));
        assert!(!is_masked("**"), "少于 3 个星号不算遮蔽");
        assert!(!is_masked("abc***"));
        assert!(!is_masked("12345678"));
    }

    /// **关键**：设备遮蔽字段时不能报「写入失败」。
    ///
    /// 这是实测踩过的坑：F650 写 WiFi 密码后回读永远是 `******`，
    /// 若按普通逻辑比对，用户会看到一个永远红色的 ❌ —— 但写入其实成功了。
    /// 那是假警报，会让人以为改坏了。
    #[test]
    fn 遮蔽字段不判为失败() {
        let t = parse_table(
            "<WLANPSK>\n  <Row No=\"0\">\n    DM name=\"KeyPassphrase\" val=\"******\"/>\n  </Row>",
        );
        let v = verdict("sendcmd 1 DB set WLANPSK 0 KeyPassphrase NewPass123", &t);
        assert_eq!(v, Verdict::Masked, "遮蔽字段应判为无法确认，而不是失败");
        // 向后兼容：verify_set 对遮蔽返回 Ok（不制造假警报）
        assert!(verify_set("sendcmd 1 DB set WLANPSK 0 KeyPassphrase NewPass123", &t).is_ok());
    }

    /// 遮蔽 ≠ 已确认。UI 必须能区分这两者，否则就是假保证。
    #[test]
    fn 遮蔽与已确认是两回事() {
        let masked = parse_table(
            "<WLANPSK>\n  <Row No=\"0\">\n    DM name=\"KeyPassphrase\" val=\"******\"/>\n  </Row>",
        );
        let real = parse_table(
            "<T>\n  <Row No=\"0\">\n    DM name=\"Pass\" val=\"NewPass123\"/>\n  </Row>",
        );
        assert_eq!(
            verdict("sendcmd 1 DB set WLANPSK 0 KeyPassphrase NewPass123", &masked),
            Verdict::Masked
        );
        assert_eq!(
            verdict("sendcmd 1 DB set T 0 Pass NewPass123", &real),
            Verdict::Confirmed
        );
    }

    /// 真失败仍要报失败，不能因为加了遮蔽逻辑就把所有不等都放过。
    #[test]
    fn 遮蔽逻辑不影响真实失败判定() {
        let t = parse_table("<T>\n  <Row No=\"0\">\n    DM name=\"F\" val=\"wrong\"/>\n  </Row>");
        assert!(matches!(
            verdict("sendcmd 1 DB set T 0 F expected", &t),
            Verdict::Failed(_)
        ));
        // 字段不存在同样是失败
        assert!(matches!(
            verdict("sendcmd 1 DB set T 0 NoSuch expected", &t),
            Verdict::Failed(_)
        ));
    }

    #[test]
    fn 字段名写错时校验失败并提示字段名() {
        // 用户把 TS_Enable 拼错，设备静默接受，回读表里根本没有这个字段
        let t = parse_table("<TelnetCfg>\n  <Row No=\"0\">\n    DM name=\"TS_Enable\" val=\"1\"/>\n  </Row>");
        let e = verify_set("sendcmd 1 DB set TelnetCfg 0 TS_Enabl 1", &t).unwrap_err();
        assert!(e.contains("TS_Enabl"), "应提示具体字段名，实际: {e}");
    }

    #[test]
    fn 行号漂移时按字段名兜底() {
        // 期望行号 0，实际在行 3 —— 值对就算生效
        let t = parse_table("<CltLmt>\n  <Row No=\"3\">\n    DM name=\"Max\" val=\"255\"/>\n  </Row>");
        assert!(verify_set("sendcmd 1 DB set CltLmt 0 Max 255", &t).is_ok());
    }

    #[test]
    fn 校验忽略大小写与首尾空白() {
        let t = parse_table("<T>\n  <Row No=\"0\">\n    DM name=\"M\" val=\"AABBCC\"/>\n  </Row>");
        assert!(verify_set("sendcmd 1 DB set T 0 M aabbcc", &t).is_ok());
        let t2 = parse_table("<T>\n  <Row No=\"0\">\n    DM name=\"M\" val=\"x\"/>\n  </Row>");
        assert!(verify_set("sendcmd 1 DB set T 0 M  x ", &t2).is_ok());
    }

    #[test]
    fn 渲染表格对齐且含表名() {
        let t = parse_table(DEVINFO);
        let s = render_table(&t);
        assert!(s.contains("DevInfo"));
        assert!(s.contains("DeviceType"));
        assert!(s.contains("G7615"));
    }

    /// 真实固件还有两种常见输出形态，字段不能被静默丢弃。
    ///
    /// 丢字段比丢表名更隐蔽：回读校验会报「回读 X 表未找到字段 Y」，
    /// 于是一个其实写成功的操作被判失败，用户去重试、去怀疑设备。
    #[test]
    fn 容器包裹与尖括号开头的DM行都能解析() {
        // 形态一：<Tbl name="X" RowCount="1"> 容器头
        let t = parse_table(
            "<Tbl name=\"DevInfo\" RowCount=\"1\">\n  <Row No=\"0\">\n    <DM name=\"ModelName\" val=\"F650\"/>\n  </Row>\n</Tbl>",
        );
        assert_eq!(t.name, "DevInfo", "<Tbl name=...> 容器头里的表名要认出来");
        assert_eq!(t.get("0", "ModelName"), Some("F650"));

        // 形态二：DM 行前面带尖括号
        let t2 = parse_table("<T>\n  <Row No=\"0\">\n    <DM name=\"A\" val=\"1\"/>\n  </Row>");
        assert_eq!(t2.get("0", "A"), Some("1"), "<DM ...> 开头的行不能被丢掉");
    }

    /// 遮蔽串的形态比 `******` 多：全角星号、圆点都见过。
    /// 少认一种的后果是「其实写成功了却报 ❌」，即假警报。
    #[test]
    fn 其他形态的遮蔽串也认得() {
        for v in ["******", "＊＊＊＊＊＊", "••••", "●●●●"] {
            assert!(is_masked(v), "{v} 应当被认成遮蔽串");
        }
        assert!(!is_masked("ab**"), "夹着正常字符就不是遮蔽");
        assert!(!is_masked("＊*"));
    }

    #[test]
    fn 全角字符按两列计宽() {
        assert_eq!(display_width("ab"), 2);
        assert_eq!(display_width("中文"), 4);
    }
}