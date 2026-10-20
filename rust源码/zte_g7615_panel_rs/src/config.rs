//! 数据驱动的命令预设：把命令清单从 Rust 代码里搬到可编辑的文本文件。
//!
//! ## 为什么要改数据驱动
//!
//! 原先每加一条命令都要改 `presets.rs` 然后重新编译。
//! 而中兴光猫的字段名在不同固件/省份版本间差异很大，
//! 用户（我）手上没有全部机型，改字段名是**必然会发生**的日常操作。
//! 把它们放进文本文件后，用户可以自己改，不必碰 Rust。
//!
//! ## 为什么不引 TOML/serde 依赖
//!
//! 本机 `cargo build --offline` 且 registry 缓存里只有 `serde`，
//! 没有 `toml` / `serde_json` / `ron`。加依赖会直接**编译不过**。
//! 所以这里写一个够用的小格式解析器，保持零依赖。
//!
//! ## 文件格式
//!
//! 纯文本，UTF-8，行式。`#` 开头是注释，空行忽略。
//!
//! ```text
//! group 状态查看
//!   action 设备信息
//!     cmd sendcmd 1 DB p DevInfo
//!   action 重启光猫 | danger | 重启期间 Telnet 会断开
//!     cmd reboot
//!   action 一键校时 | danger
//!     cmd __SET_TIME_NOW__
//! ```
//!
//! 规则：
//! - `group <名称>` 开一个分组
//! - `action <名称>[ | danger][ | <风险说明>]` 开一个操作
//!   - 第二段写 `danger` 表示危险操作（需要二次确认）
//!   - 第三段是风险说明，会显示在确认条上
//! - `param <名称>[ | <校验规则>][ | <提示文字>]` 声明一个参数
//!   - 命令里用 `$名称` 引用，执行前由界面填入
//!   - 校验规则见 `ParamKind`
//! - `cmd <命令原文>` 追加一条命令，可重复
//! - 缩进仅作可读性用，解析时忽略
//!
//! ## 参数化示例
//!
//! ```text
//! group 账号与 Telnet
//!   action 修改超级管理员密码 | danger | 改完请记牢，忘了只能重置光猫
//!     param 新密码 | password | 长度 8-32 位
//!     cmd sendcmd 1 DB set DevAuthInfo 0 Pass $新密码
//!     cmd sendcmd 1 DB save
//!
//!   action 修改 PON SN | danger | 会改变设备标识，可能被运营商拉黑
//!     param SN前四位 | sn4
//!     param SN后八位 | sn8
//!     cmd setmac 1 2176 $SN前四位
//!     cmd setmac 1 2177 $SN后八位
//! ```

use std::fmt;

/// 解析失败的位置与原因。带上行号，用户改错文件时能直接定位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: usize,
    pub msg: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "第 {} 行: {}", self.line, self.msg)
    }
}

/// 参数的校验规则。
///
/// 存在的意义是**在发到设备之前**挡住格式错误：
/// 光猫对 SN/MAC 的格式有硬性要求（长度、大小写），
/// 发错了要么静默失败、要么写进一个半吊子值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    /// 任意文本
    Text,
    /// 密码：界面上遮蔽显示
    Password,
    /// MAC 地址：12 位十六进制，允许带冒号或连字符
    Mac,
    /// SN 前四位：4 位字母数字
    Sn4,
    /// SN 后八位：8 位字母数字
    Sn8,
    /// 十进制整数
    Number,
    /// 区域码：**三位**十进制数字。
    ///
    /// 单独一个类型而不是复用 `Number`：区域码写错一位就会被
    /// `upgradetest sdefconf` 当成另一个省，设备照样重启、
    /// 照样清配置，但区域是错的。位数必须在这里就卡死。
    Region,
}

impl ParamKind {
    /// 从文件里的关键字解析。未知关键字返回 None（由调用方报错）。
    pub fn from_keyword(s: &str) -> Option<Self> {
        match s {
            "text" => Some(ParamKind::Text),
            "password" => Some(ParamKind::Password),
            "mac" => Some(ParamKind::Mac),
            "sn4" => Some(ParamKind::Sn4),
            "sn8" => Some(ParamKind::Sn8),
            "number" => Some(ParamKind::Number),
            "region" => Some(ParamKind::Region),
            _ => None,
        }
    }

    /// 关键字，用于报错提示与文档。
    pub fn keyword(self) -> &'static str {
        match self {
            ParamKind::Text => "text",
            ParamKind::Password => "password",
            ParamKind::Mac => "mac",
            ParamKind::Sn4 => "sn4",
            ParamKind::Sn8 => "sn8",
            ParamKind::Number => "number",
            ParamKind::Region => "region",
        }
    }

    /// 全部合法关键字，拼错误提示用。
    pub fn all_keywords() -> &'static str {
        "text / password / mac / sn4 / sn8 / number / region"
    }

    /// 校验一个值。返回 Err 时给的是**可直接展示给用户**的原因。
    pub fn validate(self, v: &str) -> Result<(), String> {
        let v = v.trim();
        if v.is_empty() {
            return Err("不能为空".into());
        }
        // 换行会变成设备的命令分隔符。
        //
        // `Telnet::send_line` 只把整串加 `\r\n` 写出去，不过滤内容；
        // 而参数值会被原样替换进命令模板。于是给「新 SSID」填
        // `Home\r\nreboot`，设备收到的是两条命令 —— 用户在界面上
        // 只是改了个 WiFi 名字，光猫却重启了。
        //
        // 这一条对所有类型都成立，所以放在 match 之前统一拦。
        if v.contains(['\r', '\n']) {
            return Err("不能包含换行".into());
        }
        // shell 元字符同样会拼出第二条命令。
        //
        // 换行只是最容易想到的一种：`;`、`|`、`&`、反引号、`$` 在 busybox sh 里
        // 都能开新命令或展开变量。给「新 SSID」填 `Home; reboot`，
        // 设备收到的是改 SSID + 重启两条 —— 而界面上只是改了个 WiFi 名字。
        //
        // 表名早就有白名单（见 ui::is_safe_table_name），理由是「会被拼进命令」；
        // 参数值走的是同一条拼装路径，没有理由不设同样的防线。
        //
        // 只拦「一定是语法」的几个字符，不拦空格、中文、冒号、斜杠、点号：
        // SSID、LOID、服务器地址都可能含这些，拦了就是把正常输入挡在门外。
        if let Some(c) = v.chars().find(|c| matches!(c, ';' | '|' | '&' | '`' | '$')) {
            return Err(format!(
                "不能包含 {c}（会被设备当成命令分隔或变量展开）"
            ));
        }

        match self {
            // 文本类字段（SSID、密码等）允许空格与中文，但不允许换行（上面已拦）。
            ParamKind::Text => Ok(()),
            ParamKind::Password => {
                if v.chars().count() < 8 {
                    // 光猫的密码字段一般有最小长度限制，太短的会被拒
                    return Err("密码至少 8 位".into());
                }
                Ok(())
            }
            ParamKind::Mac => {
                let hex: String = v.chars().filter(|c| *c != ':' && *c != '-').collect();
                if hex.len() != 12 {
                    return Err(format!("MAC 应为 12 位十六进制（去掉分隔符后），实际 {} 位", hex.len()));
                }
                if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
                    return Err("MAC 只能包含 0-9 a-f A-F".into());
                }
                Ok(())
            }
            ParamKind::Sn4 => check_alnum(v, 4, "SN 前四位"),
            ParamKind::Sn8 => check_alnum(v, 8, "SN 后八位"),
            ParamKind::Number => {
                if v.parse::<u64>().is_err() {
                    return Err("应为十进制整数".into());
                }
                Ok(())
            }
            ParamKind::Region => {
                if v.chars().count() != 3 || !v.chars().all(|c| c.is_ascii_digit()) {
                    return Err(format!("区域码应为三位数字（如 305），实际是「{v}」"));
                }
                Ok(())
            }
        }
    }
}

/// 字母数字且定长。
fn check_alnum(v: &str, len: usize, what: &str) -> Result<(), String> {
    if v.chars().count() != len {
        return Err(format!("{what} 应为 {len} 位，实际 {} 位", v.chars().count()));
    }
    if !v.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(format!("{what} 只能包含字母和数字"));
    }
    Ok(())
}

/// 一个参数声明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamDef {
    pub name: String,
    pub kind: ParamKind,
    /// 输入框旁的提示文字
    pub hint: Option<String>,
}

/// 一条命令预设的原始定义（与 `presets::Action` 同构，
/// 但这里是「文件里长什么样」，转换交给调用方）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionDef {
    pub name: String,
    pub danger: bool,
    /// 刷机级操作：比 danger 还要高一级。
    ///
    /// 它**隐含** `danger = true`（解析时自动置位），但界面上走的不是
    /// 普通危险确认，而是专用闸门（备份 → 三段勾选 → 双输入）。
    /// 改区域这种「会重启 + 清配置 + 掉 Telnet」的操作必须走这条路。
    pub critical: bool,
    pub note: Option<String>,
    pub cmds: Vec<String>,
    pub params: Vec<ParamDef>,
}

/// 一个分组。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupDef {
    pub name: String,
    pub actions: Vec<ActionDef>,
}

/// 解析预设文本。返回全部分组，或第一个错误（带行号）。
///
/// 之所以遇错就整体失败而不是跳过坏行：命令清单是安全相关的，
/// **静默丢掉一条用户以为存在的命令**比编译报错危险得多。
/// 用户改坏文件时会看到明确的错误行号，而不是"按钮莫名少了几个"。
pub fn parse(text: &str) -> Result<Vec<GroupDef>, ParseError> {
    let mut groups: Vec<GroupDef> = Vec::new();
    let mut cur_action: Option<ActionDef> = None;

    // 去掉 UTF-8 BOM。
    //
    // 用户会用记事本 / VS Code 改这个文件，而两者都可能存成
    // 「UTF-8 with BOM」（Windows 记事本旧版默认就是带 BOM）。
    // BOM 是 U+FEFF，**不是** char::is_whitespace，
    // 所以下面的 line.trim() 去不掉它，第一行会变成
    // "﻿group 测试组"，strip_prefix("group ") 匹配失败，
    // 整个预设文件报「无法识别的指令」而完全用不了。
    //
    // 这里在入口剥一次，比在每行 trim 里处理更稳妥。
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    for (idx, raw) in text.lines().enumerate() {
        let lineno = idx + 1;
        // 去掉注释：整行 # 开头才算，命令里的 # 要保留
        // （比如 shell 注释在 `sendcmd` 场景用不到，但 SSID 里可能有 #）
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(rest) = line.strip_prefix("group ") {
            // 开新分组前，先把上一个未收口的 action 落盘
            flush_action(&mut cur_action, &mut groups, lineno)?;
            let name = rest.trim();
            if name.is_empty() {
                return Err(ParseError { line: lineno, msg: "group 缺少名称".into() });
            }
            groups.push(GroupDef { name: name.to_string(), actions: Vec::new() });
        } else if let Some(rest) = line.strip_prefix("action ") {
            flush_action(&mut cur_action, &mut groups, lineno)?;
            let a = parse_action(rest.trim(), lineno)?;
            cur_action = Some(a);
        } else if let Some(rest) = line.strip_prefix("param ") {
            let p = parse_param(rest.trim(), lineno)?;
            match cur_action.as_mut() {
                Some(a) => {
                    // 同名参数会让 `$名称` 指向不明，必须拒绝
                    if a.params.iter().any(|x| x.name == p.name) {
                        return Err(ParseError {
                            line: lineno,
                            msg: format!("参数名 {:?} 重复", p.name),
                        });
                    }
                    a.params.push(p);
                }
                None => {
                    return Err(ParseError {
                        line: lineno,
                        msg: "param 出现在任何 action 之前".into(),
                    })
                }
            }
        } else if let Some(rest) = line.strip_prefix("cmd ") {
            let cmd = rest.trim();
            if cmd.is_empty() {
                return Err(ParseError { line: lineno, msg: "cmd 缺少命令内容".into() });
            }
            match cur_action.as_mut() {
                Some(a) => a.cmds.push(cmd.to_string()),
                None => {
                    return Err(ParseError {
                        line: lineno,
                        msg: "cmd 出现在任何 action 之前".into(),
                    })
                }
            }
        } else {
            return Err(ParseError {
                line: lineno,
                msg: format!("无法识别的指令: {line:?}（应为 group / action / param / cmd）"),
            });
        }
    }

    // 收口最后一个 action
    flush_action(&mut cur_action, &mut groups, text.lines().count().max(1))?;

    if groups.is_empty() {
        return Err(ParseError { line: 1, msg: "文件里没有任何 group".into() });
    }
    Ok(groups)
}

/// 把当前累积的 action 挂到最后一个分组下，并清空。
fn flush_action(
    cur: &mut Option<ActionDef>,
    groups: &mut [GroupDef],
    lineno: usize,
) -> Result<(), ParseError> {
    let Some(a) = cur.take() else {
        return Ok(());
    };
    let Some(g) = groups.last_mut() else {
        return Err(ParseError {
            line: lineno,
            msg: format!("action {:?} 出现在任何 group 之前", a.name),
        });
    };
    // 没有命令的 action 是配置错误：界面上会是个点了没反应的按钮。
    // 宁可报错也不让这种按钮出现。
    if a.cmds.is_empty() {
        return Err(ParseError {
            line: lineno,
            msg: format!("action {:?} 没有定义任何 cmd", a.name),
        });
    }
    // 命令里引用了没声明的参数 → 执行时会替换不掉，把 `$xxx` 原样发到设备。
    // 这是必须挡住的：设备收到的是一串乱码，而用户以为改成功了。
    for c in &a.cmds {
        for r in referenced_params(c) {
            if !a.params.iter().any(|p| p.name == r) {
                return Err(ParseError {
                    line: lineno,
                    msg: format!(
                        "action {:?} 的命令引用了未声明的参数 ${}，请补一条 param {}",
                        a.name, r, r
                    ),
                });
            }
        }
    }
    // 声明了却没用到的参数：不算致命，但多半是笔误，提示出来
    for p in &a.params {
        let used = a.cmds.iter().any(|c| referenced_params(c).contains(&p.name));
        if !used {
            return Err(ParseError {
                line: lineno,
                msg: format!(
                    "action {:?} 声明了参数 {:?} 但没有任何命令引用它",
                    a.name, p.name
                ),
            });
        }
    }
    g.actions.push(a);
    Ok(())
}

/// 抽出命令里引用的 `$参数名`。
///
/// 参数名允许中文和字母数字下划线；到「非参数名字符」为止。
/// 注意 shell 的 `$` 变量极少在光猫命令里出现，所以这里不区分对待 ——
/// 真要发一个字面 `$` 的场合目前没有。
///
/// ## 为什么 `#` 也算参数名的结束符
///
/// 光猫是 busybox，`#` 起注释。`sendcmd 1 DB set X 0 F $PIN#1` 里
/// `$PIN#1` 若被当成一个参数名，用户就得声明一个叫 `PIN#1` 的参数；
/// 而实际执行时 shell 看到的是 `sendcmd 1 DB set X 0 F 1234#1` ——
/// 也就是**值后面多了一截注释**。两边对这种写法的理解必须一致，
/// 否则「声明了却没用」或「展开后残留占位符」会以最莫名其妙的方式出现。
fn referenced_params(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = cmd;
    while let Some(i) = rest.find('$') {
        let after = &rest[i + 1..];
        let name: String = after
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || is_cjk(*c))
            .collect();
        // 占位符后面紧跟 `#` 时，`#` 是 shell 注释，不属于参数名。
        // 先记住字节长度再 move 走 name：
        // 参数名可能含中文，字节长度 != 字符个数，
        // 切 rest 必须用字节偏移，否则会切在字符中间 panic。
        let byte_len = name.len();
        if !name.is_empty() {
            out.push(name);
        }
        // 跳过这个 $ 及其参数名（名字为空时也前进，避免死循环）
        rest = &after[byte_len..];
    }
    out
}

/// 是否中日韩字符（参数名用中文更直观）。
fn is_cjk(c: char) -> bool {
    matches!(c as u32, 0x4E00..=0x9FFF | 0x3400..=0x4DBF)
}

/// 解析 `param` 行：`名称[ | 校验规则][ | 提示文字]`
fn parse_param(rest: &str, lineno: usize) -> Result<ParamDef, ParseError> {
    let parts: Vec<&str> = rest.split('|').map(|s| s.trim()).collect();
    let name = parts.first().copied().unwrap_or("");
    if name.is_empty() {
        return Err(ParseError { line: lineno, msg: "param 缺少名称".into() });
    }
    if name.contains('$') {
        return Err(ParseError {
            line: lineno,
            msg: format!("param 名称不该带 $（引用时才加），实际: {name}"),
        });
    }
    // 参数名不能含空格：`$名称` 在命令里是按「非参数名字符」截断的，
    // 带空格的声明（如 `param 原机PON MAC`）永远匹配不上
    // `$原机PON`，会在后面报出令人困惑的「引用了未声明的参数」。
    // 在这里就挡住，错误信息才指向真正的原因。
    if name.contains(char::is_whitespace) {
        return Err(ParseError {
            line: lineno,
            msg: format!(
                "param 名称不能含空格，实际: {name:?}（建议写成 原机PONMAC 这样连起来）"
            ),
        });
    }

    let mut kind = ParamKind::Text;
    let mut hint: Option<String> = None;
    // 「校验规则」是**第一个非空段**，不是「下标正好为 1 的那段」。
    //
    // 旧代码用 `i == 1` 判断，于是 `param 新密码 | | 至少 8 位`（常见的手滑）
    // 会把下标 2 的「至少 8 位」当成提示文字**静默接受**，
    // 校验规则却是默认的 text —— 密码长度限制就这么无声地消失了。
    let mut rule_seen = false;
    for p in parts.iter().skip(1) {
        if p.is_empty() {
            continue;
        }
        // 第一个非空段是校验规则，之后的是提示文字
        if !rule_seen {
            rule_seen = true;
            match ParamKind::from_keyword(p) {
                Some(k) => {
                    kind = k;
                    continue;
                }
                None => {
                    return Err(ParseError {
                        line: lineno,
                        msg: format!(
                            "未知的参数校验规则 {p:?}（可选 {}）",
                            ParamKind::all_keywords()
                        ),
                    })
                }
            }
        }
        if hint.is_none() {
            hint = Some((*p).to_string());
        } else {
            return Err(ParseError {
                line: lineno,
                msg: format!("param {name:?} 有多段提示文字，只允许一段"),
            });
        }
    }

    Ok(ParamDef { name: name.to_string(), kind, hint })
}

/// 解析 `action` 行：`名称[ | danger][ | 风险说明]`
fn parse_action(rest: &str, lineno: usize) -> Result<ActionDef, ParseError> {
    let parts: Vec<&str> = rest.split('|').map(|s| s.trim()).collect();
    let name = parts.first().copied().unwrap_or("");
    if name.is_empty() {
        return Err(ParseError { line: lineno, msg: "action 缺少名称".into() });
    }

    let mut danger = false;
    let mut critical = false;
    let mut note: Option<String> = None;
    for p in parts.iter().skip(1) {
        if *p == "danger" {
            danger = true;
        } else if *p == "critical" {
            // critical 隐含 danger：它同样会改配置，只是闸门更严。
            // 这样「危险操作永远不会被直接执行」那条测试对 critical 也成立。
            danger = true;
            critical = true;
        } else if p.is_empty() {
            // 允许 `action X | danger |` 这种尾随空段
            continue;
        } else if note.is_none() {
            note = Some((*p).to_string());
        } else {
            return Err(ParseError {
                line: lineno,
                msg: format!("action {name:?} 有多段说明，只允许一段风险说明"),
            });
        }
    }

    // 非危险操作不该带说明：说明只在确认条上显示，没有确认条就没地方显示，
    // 静默丢掉会让用户以为说明生效了。
    if !danger && note.is_some() {
        return Err(ParseError {
            line: lineno,
            msg: format!("action {name:?} 带了风险说明但没有 danger 标记"),
        });
    }

    Ok(ActionDef {
        name: name.to_string(),
        danger,
        critical,
        note,
        cmds: Vec::new(),
        params: Vec::new(),
    })
}

/// 把命令里的 `$参数名` 替换成实际值。
///
/// `values` 未提供的参数保持原样不替换（调用方应先校验齐全再调用）。
/// 替换按「长名优先」进行，避免 `$SN` 把 `$SN前四位` 的前缀吃掉。
pub fn substitute(cmd: &str, values: &[(String, String)]) -> String {
    let mut pairs: Vec<&(String, String)> = values.iter().collect();
    pairs.sort_by_key(|(k, _)| std::cmp::Reverse(k.len()));

    let mut out = cmd.to_string();
    for (k, v) in pairs {
        out = out.replace(&format!("${k}"), v);
    }
    out
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    /// 参数值里的换行必须被拒绝 —— 它会变成第二条命令。
    ///
    /// `Telnet::send_line` 不过滤内容，参数值又被原样替换进命令模板，
    /// 所以给 SSID 填 `Home\r\nreboot` 会让光猫多执行一条重启命令。
    #[test]
    fn 参数值不接受换行() {
        // 所有类型都拦
        for kind in [
            ParamKind::Text,
            ParamKind::Password,
            ParamKind::Mac,
            ParamKind::Sn4,
            ParamKind::Sn8,
            ParamKind::Number,
            ParamKind::Region,
        ] {
            for bad in ["Home\r\nreboot", "Home\nreboot", "Home\rreboot"] {
                assert!(
                    kind.validate(bad).is_err(),
                    "{:?} 不该接受 {bad:?}",
                    kind
                );
            }
        }
        // shell 元字符同样能拼出第二条命令，必须一起拦
        for bad in [
            "Home; reboot",
            "Home|reboot",
            "Home&&reboot",
            "`reboot`",
            "$(reboot)",
        ] {
            assert!(
                ParamKind::Text.validate(bad).is_err(),
                "文本参数不该接受 {bad:?}"
            );
        }
        // 但正常值里可能出现的符号不能被误伤
        for ok in [
            "My Home WiFi",
            "http://127.0.0.1",
            "aa:bb:cc:dd:ee:ff",
            "LOID-2026_01",
            "我的 WiFi 5G",
        ] {
            assert!(
                ParamKind::Text.validate(ok).is_ok(),
                "{ok:?} 是正常输入，不该被拦"
            );
        }

        // 正常的带空格 / 中文的值仍然放行
        assert!(ParamKind::Text.validate("My Home WiFi").is_ok());
        assert!(ParamKind::Text.validate("我的 WiFi 5G").is_ok());
        assert!(ParamKind::Password.validate("Zte521ab").is_ok());
    }

    /// 带 UTF-8 BOM 的预设文件必须能正常解析。
    ///
    /// 这不是假想场景：Windows 记事本默认就存带 BOM 的 UTF-8，
    /// VS Code 的「UTF-8 with BOM」也是常见选择。
    /// 修复前整个文件会报「无法识别的指令」而完全不可用。
    #[test]
    fn 带BOM的预设文件能正常解析() {
        let text = "\u{feff}group 测试组\n  action 测试操作\n    cmd echo hi\n";
        let g = parse(text).expect("带 BOM 的文件必须能解析");
        assert_eq!(g.len(), 1);
        assert_eq!(g[0].name, "测试组");
        assert_eq!(g[0].actions[0].cmds, vec!["echo hi"]);
        // 不带 BOM 的结果必须完全一致
        let plain = "group 测试组\n  action 测试操作\n    cmd echo hi\n";
        assert_eq!(parse(plain).unwrap(), g);
    }

    use super::*;

    const SAMPLE: &str = r#"
# 这是注释
group 状态查看
  action 设备信息
    cmd sendcmd 1 DB p DevInfo
  action 运行时长
    cmd uptime

group 系统
  action 重启光猫 | danger | 重启期间 Telnet 会断开
    cmd reboot
"#;

    #[test]
    fn 解析分组与命令() {
        let g = parse(SAMPLE).unwrap();
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].name, "状态查看");
        assert_eq!(g[0].actions.len(), 2);
        assert_eq!(g[0].actions[0].name, "设备信息");
        assert_eq!(g[0].actions[0].cmds, vec!["sendcmd 1 DB p DevInfo"]);
        assert!(!g[0].actions[0].danger);
        assert_eq!(g[1].name, "系统");
    }

    /// critical 关键字：比 danger 高一级，且**隐含** danger。
    ///
    /// 隐含是刻意的：critical 操作同样会改配置，
    /// 「危险操作永远不会被直接执行」那条不变量对它也必须成立。
    #[test]
    fn critical隐含danger且被标记() {
        let t = "group 区域\n  action 改区 | critical | 会重启\n    cmd upgradetest sdefconf 210\n";
        let g = parse(t).unwrap();
        let a = &g[0].actions[0];
        assert!(a.critical, "critical 标记没解析出来");
        assert!(a.danger, "critical 必须隐含 danger");
        assert_eq!(a.note.as_deref(), Some("会重启"));
    }

    /// 旧写法必须完全不受影响 —— 加了新关键字不能改变 danger 的语义。
    #[test]
    fn 旧danger行为不变() {
        let g = parse(SAMPLE).unwrap();
        assert!(!g[0].actions[0].critical);
        assert!(g[1].actions[0].danger);
        assert!(!g[1].actions[0].critical, "普通 danger 不该被当成 critical");
    }

    /// region 参数类型：必须是三位数字。
    #[test]
    fn 区域码必须是三位数字() {
        assert!(ParamKind::Region.validate("210").is_ok());
        assert!(ParamKind::Region.validate("305").is_ok());
        for bad in ["21", "2100", "abc", "21a", "", " 2 "] {
            assert!(
                ParamKind::Region.validate(bad).is_err(),
                "{bad:?} 不该通过区域码校验"
            );
        }
    }

    #[test]
    fn region关键字可解析且可回写() {
        assert_eq!(ParamKind::from_keyword("region"), Some(ParamKind::Region));
        assert_eq!(ParamKind::Region.keyword(), "region");
        assert!(ParamKind::all_keywords().contains("region"));
    }

    #[test]
    fn 危险操作带风险说明() {
        let g = parse(SAMPLE).unwrap();
        let reboot = &g[1].actions[0];
        assert_eq!(reboot.name, "重启光猫");
        assert!(reboot.danger);
        assert_eq!(reboot.note.as_deref(), Some("重启期间 Telnet 会断开"));
    }

    #[test]
    fn 多条命令按顺序累积() {
        let t = "group A\n  action B\n    cmd one\n    cmd two\n    cmd three\n";
        let g = parse(t).unwrap();
        assert_eq!(g[0].actions[0].cmds, vec!["one", "two", "three"]);
    }

    #[test]
    fn 命令里的特殊字符被保留() {
        // SSID 里可能有 # 和 |，不能被当注释或分隔符吃掉
        let t = "group A\n  action B\n    cmd sendcmd 1 DB set WLANCfg 0 ESSID My#Net|5G\n";
        let g = parse(t).unwrap();
        assert_eq!(g[0].actions[0].cmds[0], "sendcmd 1 DB set WLANCfg 0 ESSID My#Net|5G");
    }

    #[test]
    fn 空文件报错() {
        let e = parse("").unwrap_err();
        assert!(e.msg.contains("group"), "实际: {}", e.msg);
    }

    #[test]
    fn 只有注释报错() {
        let e = parse("# 什么都没有\n\n").unwrap_err();
        assert!(e.msg.contains("group"));
    }

    #[test]
    fn 无法识别的指令带行号() {
        let t = "group A\n  action B\n    cmd x\n    bogus y\n";
        let e = parse(t).unwrap_err();
        assert_eq!(e.line, 4);
        assert!(e.msg.contains("无法识别"), "实际: {}", e.msg);
    }

    #[test]
    fn action没有cmd时报错并带行号() {
        // 这种会变成「点了没反应的按钮」，必须拒绝
        let t = "group A\n  action 空操作\n  action 有命令\n    cmd x\n";
        let e = parse(t).unwrap_err();
        assert!(e.msg.contains("没有定义任何 cmd"), "实际: {}", e.msg);
        assert!(e.msg.contains("空操作"));
    }

    #[test]
    fn cmd出现在action之前报错() {
        let t = "group A\n  cmd 游离命令\n";
        let e = parse(t).unwrap_err();
        assert!(e.msg.contains("之前"), "实际: {}", e.msg);
    }

    #[test]
    fn action出现在group之前报错() {
        let t = "action 游离\n  cmd x\n";
        let e = parse(t).unwrap_err();
        assert!(e.msg.contains("group"), "实际: {}", e.msg);
    }

    #[test]
    fn 非危险操作带说明报错() {
        // 说明只在确认条上显示；非危险操作没有确认条，说明会被静默丢弃
        let t = "group A\n  action B | 这段说明没地方显示\n    cmd x\n";
        let e = parse(t).unwrap_err();
        assert!(e.msg.contains("danger"), "实际: {}", e.msg);
    }

    #[test]
    fn 多段说明报错() {
        let t = "group A\n  action B | danger | 说明一 | 说明二\n    cmd x\n";
        let e = parse(t).unwrap_err();
        assert!(e.msg.contains("只允许一段"), "实际: {}", e.msg);
    }

    #[test]
    fn 危险的空说明被容忍() {
        let t = "group A\n  action B | danger |\n    cmd x\n";
        let g = parse(t).unwrap();
        assert!(g[0].actions[0].danger);
        assert!(g[0].actions[0].note.is_none());
    }

    #[test]
    fn 缩进不影响解析() {
        let a = parse("group A\naction B\ncmd x\n").unwrap();
        let b = parse("group A\n      action B\n            cmd x\n").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn 最后一个action能正确收口() {
        // 文件末尾没有换行也要收口
        let g = parse("group A\n  action B\n    cmd x").unwrap();
        assert_eq!(g[0].actions.len(), 1);
        assert_eq!(g[0].actions[0].cmds, vec!["x"]);
    }

    // ---------- 参数化 ----------

    #[test]
    fn 解析参数声明() {
        let t = "group A\n  action B\n    param 新密码 | password | 长度 8-32 位\n    cmd sendcmd 1 DB set X 0 Pass $新密码\n";
        let g = parse(t).unwrap();
        let a = &g[0].actions[0];
        assert_eq!(a.params.len(), 1);
        assert_eq!(a.params[0].name, "新密码");
        assert_eq!(a.params[0].kind, ParamKind::Password);
        assert_eq!(a.params[0].hint.as_deref(), Some("长度 8-32 位"));
    }

    /// 校验规则是「第一个非空段」，不是「下标正好为 1 的那段」。
    ///
    /// 「校验规则」是第一个**非空**段，不是下标正好为 1 的那段。
    ///
    /// 旧代码用 `i == 1` 判断，于是 `param 新密码 | | password`（中间多敲了一个
    /// 竖线）会走到下标 2：既不认它是规则，也不报错，而是把它当成**提示文字**
    /// 收下，校验规则静默退回默认的 `text` —— 密码长度限制就此消失，
    /// 文件解析却显示成功。
    #[test]
    fn 空段跳过后第一个非空段才是规则() {
        let t3 = "group A\n  action B\n    param 新密码 | | password\n    cmd sendcmd 1 DB set X 0 Pass $新密码\n";
        let g3 = parse(t3).unwrap();
        assert_eq!(
            g3[0].actions[0].params[0].kind,
            ParamKind::Password,
            "空段之后的第一段就是规则，不能被当成提示文字"
        );
        // 提示文字仍然在规则之后的那一段
        let t4 = "group A\n  action B\n    param 新密码 | | password | 至少 8 位\n    cmd sendcmd 1 DB set X 0 Pass $新密码\n";
        let g4 = parse(t4).unwrap();
        assert_eq!(g4[0].actions[0].params[0].kind, ParamKind::Password);
        assert_eq!(g4[0].actions[0].params[0].hint.as_deref(), Some("至少 8 位"));
    }

    #[test]
    fn 不带校验规则的参数默认是文本() {
        let t = "group A\n  action B\n    param 名字\n    cmd echo $名字\n";
        let g = parse(t).unwrap();
        assert_eq!(g[0].actions[0].params[0].kind, ParamKind::Text);
        assert!(g[0].actions[0].params[0].hint.is_none());
    }

    #[test]
    fn 未知校验规则报错() {
        let t = "group A\n  action B\n    param X | bogus\n    cmd echo $X\n";
        let e = parse(t).unwrap_err();
        assert!(e.msg.contains("未知的参数校验规则"), "实际: {}", e.msg);
        assert!(e.msg.contains("password"), "应列出可选值，实际: {}", e.msg);
    }

    /// **关键**：命令引用了没声明的参数 → 必须报错。
    ///
    /// 否则执行时替换不掉，`$未声明` 会被原样发到设备，
    /// 用户以为改成功了，实际设备收到的是一串乱码。
    #[test]
    fn 引用未声明参数报错() {
        let t = "group A\n  action B\n    cmd sendcmd 1 DB set X 0 Pass $没声明过\n";
        let e = parse(t).unwrap_err();
        assert!(e.msg.contains("未声明的参数"), "实际: {}", e.msg);
        assert!(e.msg.contains("没声明过"), "应指出参数名，实际: {}", e.msg);
    }

    /// 声明了却没用到的参数多半是笔误，也要报错。
    #[test]
    fn 声明了未使用的参数报错() {
        let t = "group A\n  action B\n    param 没用到\n    cmd echo hello\n";
        let e = parse(t).unwrap_err();
        assert!(e.msg.contains("没有任何命令引用"), "实际: {}", e.msg);
        assert!(e.msg.contains("没用到"), "应指出参数名，实际: {}", e.msg);
    }

    #[test]
    fn 参数名重复报错() {
        let t = "group A\n  action B\n    param X\n    param X | number\n    cmd echo $X\n";
        let e = parse(t).unwrap_err();
        assert!(e.msg.contains("重复"), "实际: {}", e.msg);
    }

    #[test]
    fn 参数名不该带美元符() {
        let t = "group A\n  action B\n    param $X\n    cmd echo $X\n";
        let e = parse(t).unwrap_err();
        assert!(e.msg.contains("不该带 $"), "实际: {}", e.msg);
    }

    /// 参数名含空格必须在**声明处**就报错。
    ///
    /// 否则 `param 原机PON MAC` 声明成功，而命令里的 `$原机PON`
    /// 会被当成未声明的参数，报出令人困惑的错误 ——
    /// 真正的原因（名字里有空格）被掩盖了。
    #[test]
    fn 参数名含空格在声明处报错() {
        let t = "group A\n  action B\n    param 原机PON MAC | mac\n    cmd setmac 1 32769 $原机PON\n";
        let e = parse(t).unwrap_err();
        assert!(
            e.msg.contains("不能含空格"),
            "应指出名称含空格，实际: {}",
            e.msg
        );
        assert!(
            !e.msg.contains("未声明"),
            "不该报成「引用了未声明的参数」，实际: {}",
            e.msg
        );
    }

    /// 抽出命令里引用的参数名（含中文名，注意字节 vs 字符）。
    #[test]
    fn 抽出引用参数名() {
        assert_eq!(referenced_params("echo $A"), vec!["A"]);
        assert_eq!(referenced_params("a $B b $C"), vec!["B", "C"]);
        // 中文参数名：字节长度 != 字符个数，切分必须用字节偏移
        assert_eq!(referenced_params("setmac 1 2176 $SN前四位"), vec!["SN前四位"]);
        assert_eq!(
            referenced_params("x $中1 y $中2"),
            vec!["中1", "中2"],
            "多个中文参数名不能互相吃掉"
        );
        // 光秃秃的 $ 不能让循环卡住
        assert_eq!(referenced_params("echo $"), Vec::<String>::new());
        assert_eq!(referenced_params("echo $$A"), vec!["A"]);
    }

    #[test]
    fn 替换参数值() {
        let v = vec![("甲".to_string(), "1".to_string())];
        assert_eq!(substitute("echo $甲", &v), "echo 1");
    }

    /// 长名优先：`$SN前四位` 不能被 `$SN` 的错误前缀替换吃掉。
    #[test]
    fn 替换时长参数名优先() {
        let v = vec![
            ("SN".to_string(), "SHORT".to_string()),
            ("SN前四位".to_string(), "ABCD".to_string()),
        ];
        assert_eq!(
            substitute("setmac 1 2176 $SN前四位", &v),
            "setmac 1 2176 ABCD",
            "长名必须优先替换"
        );
    }

    #[test]
    fn 未提供的参数保持原样() {
        let v: Vec<(String, String)> = vec![];
        assert_eq!(substitute("echo $X", &v), "echo $X");
    }

    // ---------- 参数校验 ----------

    #[test]
    fn 文本参数接受任意非空值() {
        assert!(ParamKind::Text.validate("随便什么").is_ok());
        assert!(ParamKind::Text.validate("").is_err());
        assert!(ParamKind::Text.validate("   ").is_err(), "全空白应视为空");
    }

    #[test]
    fn 密码参数要求最小长度() {
        assert!(ParamKind::Password.validate("12345678").is_ok());
        assert!(ParamKind::Password.validate("1234567").is_err(), "7 位应被拒");
    }

    #[test]
    fn MAC参数接受带分隔符与不带分隔符() {
        for v in ["aabbccddeeff", "AA:BB:CC:DD:EE:FF", "aa-bb-cc-dd-ee-ff"] {
            assert!(ParamKind::Mac.validate(v).is_ok(), "{v} 应通过");
        }
        assert!(ParamKind::Mac.validate("aabbccddee").is_err(), "10 位应被拒");
        assert!(ParamKind::Mac.validate("aabbccddeegg").is_err(), "非十六进制应被拒");
    }

    #[test]
    fn SN参数校验长度与字符集() {
        assert!(ParamKind::Sn4.validate("ABCD").is_ok());
        assert!(ParamKind::Sn4.validate("ABC").is_err());
        assert!(ParamKind::Sn8.validate("ABCD1234").is_ok());
        assert!(ParamKind::Sn8.validate("ABCD123").is_err());
        // 中文不该通过 SN 校验
        assert!(ParamKind::Sn4.validate("中文中文").is_err());
    }

    #[test]
    fn 数字参数只收十进制整数() {
        assert!(ParamKind::Number.validate("255").is_ok());
        assert!(ParamKind::Number.validate("0").is_ok());
        assert!(ParamKind::Number.validate("abc").is_err());
        assert!(ParamKind::Number.validate("-1").is_err(), "负数不该通过");
    }

    #[test]
    fn 校验规则关键字可往返() {
        for k in ["text", "password", "mac", "sn4", "sn8", "number"] {
            let kind = ParamKind::from_keyword(k).expect(k);
            assert_eq!(kind.keyword(), k);
        }
        assert!(ParamKind::from_keyword("bogus").is_none());
    }
}
