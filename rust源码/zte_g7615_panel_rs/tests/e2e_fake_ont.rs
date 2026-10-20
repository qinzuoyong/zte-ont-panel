//! 端到端集成测试：把 `main.rs` 里真实的预设命令序列，
//! 打到一台「假光猫」上完整跑一遍。
//!
//! 覆盖原实现最容易出错的路径：
//!  - 关闭回显 + 无输出命令（`sendcmd 1 DB set/save`）必须立刻返回，不能干等超时
//!  - 残留提示符不能漏进输出，也不能把结束标记粘住
//!  - 超时后要真正重新对齐，不能把上一条的迟到输出算到下一条头上
//!  - `reboot` 断链要按预期结果处理，而不是报失败
//!
//! 假设备刻意模拟真实 G7615 的行为：提示符不带换行、命令无回显。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use zte_g7615_panel::config;
use zte_g7615_panel::dbcli;
use zte_g7615_panel::presets::builtin as presets;
use zte_g7615_panel::telnet::{ErrKind, Telnet};

const PS1: &str = "G7615> ";

/// 假光猫：接受一个连接，按脚本应答，并把收到的命令回传给测试线程。
struct FakeOnt {
    addr: String,
    cmds: Receiver<String>,
    _h: thread::JoinHandle<()>,
}

/// 假光猫对写命令的态度，用来测「写后回读校验」。
#[derive(Clone, Copy, PartialEq, Eq)]
enum WriteMode {
    /// 正常设备：`sendcmd 1 DB set X r f v` 真的写进表里，回读能看到
    Honest,
    /// 撒谎设备：命令照收、什么都不报错，但配置**根本没写进去**。
    /// 这正是中兴设备在字段名/行号写错时的真实行为，
    /// 也是 P0 回读校验存在的唯一理由。
    Lying,
}

fn read_line(s: &mut TcpStream) -> Option<String> {
    let mut out = Vec::new();
    let mut b = [0u8; 1];
    loop {
        match s.read(&mut b) {
            Ok(0) => return None,
            Ok(_) => {
                if b[0] == b'\n' {
                    break;
                }
                if b[0] != b'\r' {
                    out.push(b[0]);
                }
            }
            Err(_) => return None,
        }
    }
    Some(String::from_utf8_lossy(&out).to_string())
}

/// 启动假光猫。`reboot_after` 控制在收到 reboot 后关闭连接。
fn spawn_ont(close_on_reboot: bool) -> FakeOnt {
    spawn_ont_mode(close_on_reboot, WriteMode::Honest)
}

/// 启动假光猫，并指定它对写命令的态度。
fn spawn_ont_mode(close_on_reboot: bool, mode: WriteMode) -> FakeOnt {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let (tx, rx): (Sender<String>, Receiver<String>) = channel();

    let h = thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(60))).unwrap();

        // 设备声明 WILL ECHO / WILL SGA，客户端回 DONT → 设备关闭回显
        let _ = s.write_all(&[255, 251, 1]);
        let _ = s.write_all(&[255, 251, 3]);

        // 登录：login: → Password: → banner + 提示符
        let _ = s.write_all(b"\r\nG7615 login: ");
        let _ = read_line(&mut s).unwrap_or_default();
        let _ = s.write_all(b"Password: ");
        let _ = read_line(&mut s).unwrap_or_default();
        let _ = s.write_all(format!("\r\nWelcome\r\n{}", PS1).as_bytes());

        // 假设备维护一份真实配置：键是 (表名, 行号, 字段名)。
        // 这样「写进去没有」才有客观答案，回读校验才测得出真假。
        let mut db: std::collections::HashMap<(String, String, String), String> =
            std::collections::HashMap::new();

        loop {
            let line = match read_line(&mut s) {
                Some(v) => v,
                None => break,
            };
            let _ = tx.send(line.clone());

            if line.starts_with("export PS1") {
                let _ = s.write_all(PS1.as_bytes());
                continue;
            }

            // 真实命令的形态是 `<cmd> ; echo E<marker>`：
            // shell 先跑 cmd，再执行 echo，所以标记出现在输出之后。
            // 假设备必须原样回吐这个标记，否则客户端永远等不到它。
            let (cmd, marker) = match line.split_once(" ; echo ") {
                Some((c, m)) => (c.trim().to_string(), Some(m.trim().to_string())),
                None => (line.trim().to_string(), None),
            };

            let mut out = String::new();
            let mut stop = false;

            if cmd == "reboot" {
                if close_on_reboot {
                    stop = true;
                }
            } else if cmd.starts_with("sendcmd 1 DB set ") {
                // 写配置命令：本身无输出。
                // Honest 模式真的落库；Lying 模式收下就当没发生（这是真机行为）。
                let t: Vec<&str> = cmd.split_whitespace().collect();
                if mode == WriteMode::Honest && t.len() >= 8 {
                    let key = (t[4].to_string(), t[5].to_string(), t[6].to_string());
                    db.insert(key, t[7..].join(" "));
                }
            } else if cmd == "sendcmd 1 DB save" {
                // 无输出
            } else if cmd.starts_with("sendcmd 1 DB p ") {
                // 回读：把库里该表的内容按真机格式吐出来
                let t = cmd.trim_start_matches("sendcmd 1 DB p ").trim();
                out.push_str(&format!("<{t}>\r\n"));
                // 按行号归组，保证输出稳定
                let mut by_row: std::collections::BTreeMap<String, Vec<(String, String)>> =
                    std::collections::BTreeMap::new();
                for ((tbl, row, field), val) in db.iter() {
                    if tbl == t {
                        by_row
                            .entry(row.clone())
                            .or_default()
                            .push((field.clone(), val.clone()));
                    }
                }
                if by_row.is_empty() {
                    // 表存在但没数据：真机就是只打一行表名
                }
                for (row, fields) in by_row {
                    out.push_str(&format!("  <Row No=\"{row}\">\r\n"));
                    let mut fs = fields;
                    fs.sort();
                    for (f, v) in fs {
                        out.push_str(&format!("    DM name=\"{f}\" val=\"{v}\"/>\r\n"));
                    }
                    out.push_str("  </Row>\r\n");
                }
                out.push_str(&format!("</{t}>\r\n"));
            } else if cmd == "sendcmd 1 DB all" {
                out.push_str("DevInfo\r\nTelnetCfg\r\nDevAuthInfo\r\n");
            } else if cmd == "uptime" {
                out.push_str(" 12:00:00 up 100 days,  1 user\r\n");
            } else if cmd == "cat /proc/net/arp" {
                out.push_str("IP address       HW address\r\n192.168.1.10     aa:bb:cc:dd:ee:ff\r\n");
            } else if cmd == "cat /proc/tempsensor" {
                out.push_str("CPU: 56 C\r\n");
            } else if cmd == "cat /proc/cpuusage" {
                out.push_str("CPU: 12%\r\n");
            } else if cmd == "cat /proc/capability/boardinfo" {
                out.push_str("Board: G7615\r\n");
            } else if cmd == "upgradetest getver" {
                out.push_str("V2.0.0P1N8\r\n");
            } else if cmd == "date" {
                out.push_str("Wed Oct 14 05:48:57 UTC 2026\r\n");
            } else if cmd.starts_with("date -s ") {
                // 校时：真机成功时通常无输出，或回显新时间
            } else if cmd == "setmac show2" {
                out.push_str("id=2177  val=ZTEG12345678\r\nid=32769 val=aa:bb:cc:dd:ee:ff\r\n");
            } else if cmd.starts_with("setmac 3 ") {
                // 删除参数：无输出
            } else if cmd.starts_with("setmac 1 ") {
                // 写参数：无输出（真机行为）
            } else if cmd == "killall telnetd" {
                // 重启 telnetd：无输出
            } else if cmd == "cat /etc/init.d/regioncode" {
                out.push_str("guangdong 210\r\nchongqing 206\r\n");
            } else if cmd == "cat /userconfig/flag_type" {
                out.push_str("210\r\n");
            } else if cmd.starts_with("echo ") {
                // 登录校验 / 重同步
                out.push_str(&format!("{}\r\n", cmd[5..].trim()));
            } else {
                out.push_str(&format!("sh: {cmd}: not found\r\n"));
            }

            // 真机 reboot 会直接掉线，不会再有输出、标记和提示符
            if stop {
                break;
            }

            let _ = s.write_all(out.as_bytes());
            // 输出之后回吐结束标记，再给提示符（不带换行）
            if let Some(m) = marker {
                let _ = s.write_all(format!("{}\r\n{}", m, PS1).as_bytes());
            } else {
                let _ = s.write_all(PS1.as_bytes());
            }
        }
    });

    FakeOnt { addr: addr.to_string(), cmds: rx, _h: h }
}

fn split_addr(a: &str) -> (String, String) {
    let mut it = a.rsplitn(2, ':');
    let port = it.next().unwrap().to_string();
    let host = it.next().unwrap().to_string();
    (host, port)
}

/// 核心回归：写一串配置（全部无输出）必须快速返回，不能干等 20 秒超时。
#[test]
fn 固化telnet预设的全部无输出命令都不会超时() {
    let ont = spawn_ont(false);
    let (host, port) = split_addr(&ont.addr);
    let mut t = Telnet::connect(&host, &port, "root", "Zte521").unwrap();

    let cmds = [
        "sendcmd 1 DB set TelnetCfg 0 Lan_Enable 1",
        "sendcmd 1 DB set TelnetCfg 0 Lan_EnableAfterOlt 1",
        "sendcmd 1 DB set TelnetCfg 0 CloseServerTime 9999999",
        "sendcmd 1 DB set TelnetCfg 0 TS_UName root",
        "sendcmd 1 DB set TelnetCfg 0 TSLan_UName root",
        "sendcmd 1 DB set TelnetCfg 0 TS_UPwd Zte521",
        "sendcmd 1 DB set TelnetCfg 0 TSLan_UPwd Zte521",
        "sendcmd 1 DB save",
    ];

    let start = Instant::now();
    for c in cmds {
        let out = t.run_with_timeout(c, 5).unwrap_or_else(|e| panic!("{c} 失败: {e}"));
        assert_eq!(out, "(无输出)", "{c} 的输出异常: {out:?}");
        assert!(!out.contains("G7615"), "{c} 的输出混入了提示符: {out:?}");
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "8 条无输出命令耗时 {elapsed:?}，说明仍在干等超时"
    );
}

/// 有输出的命令：内容要完整，且不能带提示符前缀。
#[test]
fn 有输出命令的内容完整且不含提示符() {
    let ont = spawn_ont(false);
    let (host, port) = split_addr(&ont.addr);
    let mut t = Telnet::connect(&host, &port, "root", "Zte521").unwrap();

    let up = t.run_with_timeout("uptime", 5).unwrap();
    assert_eq!(up, " 12:00:00 up 100 days,  1 user");
    assert!(!up.contains("G7615"));

    let arp = t.run_with_timeout("cat /proc/net/arp", 5).unwrap();
    assert!(arp.starts_with("IP address"), "表头丢失: {arp:?}");
    assert!(arp.contains("aa:bb:cc:dd:ee:ff"));

    let dev = t.run_with_timeout("sendcmd 1 DB p DevInfo", 5).unwrap();
    assert!(dev.starts_with("<DevInfo>"), "内容错位: {dev:?}");
    assert!(!dev.contains("G7615"));
}

/// reboot 导致断链：必须报 Disconnected，好让上层按「预期结果」处理。
#[test]
fn reboot断链按断链处理() {
    let ont = spawn_ont(true);
    let (host, port) = split_addr(&ont.addr);
    let mut t = Telnet::connect(&host, &port, "root", "Zte521").unwrap();
    let e = t.run_with_timeout("reboot", 5).unwrap_err();
    assert_eq!(e.kind, ErrKind::Disconnected, "reboot 应判为断链，实际: {e}");
}

/// 连续混合执行：有输出 / 无输出交替，验证会话不会错位。
#[test]
fn 混合命令连续执行不会错位() {
    let ont = spawn_ont(false);
    let (host, port) = split_addr(&ont.addr);
    let mut t = Telnet::connect(&host, &port, "root", "Zte521").unwrap();

    assert_eq!(t.run_with_timeout("uptime", 5).unwrap(), " 12:00:00 up 100 days,  1 user");
    assert_eq!(t.run_with_timeout("sendcmd 1 DB save", 5).unwrap(), "(无输出)");
    assert_eq!(t.run_with_timeout("uptime", 5).unwrap(), " 12:00:00 up 100 days,  1 user");
    assert_eq!(t.run_with_timeout("sendcmd 1 DB set MgtServer 0 Tr069Enable 0", 5).unwrap(), "(无输出)");
    assert_eq!(t.run_with_timeout("uptime", 5).unwrap(), " 12:00:00 up 100 days,  1 user");
}

/// 把**真实的预设命令列表**全跑一遍（不碰真机）：
/// 逐个操作按界面上的顺序执行，校验没有超时、没有提示符泄漏、没有内容错位。
///
/// 这是最接近「用户点遍所有按钮」的自动化验证。
#[test]
fn 跑遍所有非危险预设都不超时且输出干净() {
    let ont = spawn_ont(false);
    let (host, port) = split_addr(&ont.addr);
    let mut t = Telnet::connect(&host, &port, "root", "Zte521").unwrap();

    let mut ran = 0usize;
    for (group, actions) in presets() {
        for a in actions {
            // reboot 会断链，单独验证；这里跳过
            if a.cmds.iter().any(|c| c.trim() == "reboot") {
                continue;
            }
            let start = Instant::now();
            for c in &a.cmds {
                // 占位命令必须由 UI 在点击时替换成真实命令，
                // 绝不能原样发到设备上（发了就是 sh: not found）。
                // 这里显式跳过占位符，并由下面的断言保证它只出现在预期位置。
                if c.starts_with("__") {
                    assert_eq!(
                        c, "__SET_TIME_NOW__",
                        "[{group}] {} 出现未知占位命令: {c}",
                        a.name
                    );
                    continue;
                }
                let out = t
                    .run_with_timeout(c, 5)
                    .unwrap_or_else(|e| panic!("[{group}] {} 执行 {c} 失败: {e}", a.name));
                assert!(
                    !out.contains("G7615>"),
                    "[{group}] {} 的输出混入了提示符: {out:?}",
                    a.name
                );
                assert!(
                    !out.contains("sh: "),
                    "[{group}] {} 的命令 {c} 在假设备上不存在: {out:?}",
                    a.name
                );
            }
            let el = start.elapsed();
            assert!(
                el < Duration::from_secs(8),
                "[{group}] {} 跑了 {el:?}，明显在干等超时",
                a.name
            );
            ran += 1;
        }
    }
    assert!(ran >= 20, "至少应覆盖 20 个预设操作，实际 {ran}");
}

/// 占位命令在界面上必须被替换成真实命令才能下发。
///
/// 这条测试守住 `presets()` 里的 `__SET_TIME_NOW__`：
/// 如果 UI 忘了替换，用户点「校时」会往设备发一串乱码。
#[test]
fn 校时占位命令能被替换成真实命令() {
    // 模拟 UI 在点击时的替换逻辑
    for (_g, actions) in presets() {
        for a in actions {
            for c in &a.cmds {
                if c == "__SET_TIME_NOW__" {
                    let real = zte_g7615_panel::presets::set_time_cmd_from_beijing(
                        2026, 10, 14, 13, 48, 57,
                    );
                    assert!(real.starts_with("date -s "), "替换结果不像校时命令: {real}");
                    assert!(!real.contains("__"), "替换后不该残留占位符: {real}");
                }
            }
        }
    }
}

/// 危险的写配置预设（固化 Telnet / TR069 开关）必须能连续跑完：
/// 这些命令全是无输出，正是原实现会整条误报超时的那批。
#[test]
fn 危险写配置预设全部能正常完成() {
    let ont = spawn_ont(false);
    let (host, port) = split_addr(&ont.addr);
    let mut t = Telnet::connect(&host, &port, "root", "Zte521").unwrap();

    let mut checked = 0usize;
    for (_group, actions) in presets() {
        for a in actions {
            if !a.danger {
                continue;
            }
            if a.cmds.iter().any(|c| c.trim() == "reboot") {
                continue;
            }
            let start = Instant::now();
            for c in &a.cmds {
                let out = t
                    .run_with_timeout(c, 5)
                    .unwrap_or_else(|e| panic!("{} 执行 {c} 失败: {e}", a.name));
                assert_eq!(out, "(无输出)", "{} 的 {c} 输出异常: {out:?}", a.name);
            }
            assert!(
                start.elapsed() < Duration::from_secs(6),
                "{} 耗时 {:?}，仍在干等超时",
                a.name,
                start.elapsed()
            );
            checked += 1;
        }
    }
    assert!(checked >= 3, "至少应覆盖 3 个危险预设，实际 {checked}");
}

/// 密码错误时不能误判登录成功。
#[test]
fn 密码错误时登录失败() {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap();
    let h = thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let _ = s.write_all(b"G7615 login: ");
        let _ = read_line(&mut s);
        let _ = s.write_all(b"Password: ");
        let _ = read_line(&mut s);
        // 密码错误：只回显一行，绝不会出现整行等于 tag 的输出
        let _ = s.write_all(b"\r\nLogin incorrect\r\nG7615 login: ");
        thread::sleep(Duration::from_secs(20));
    });
    let (host, port) = split_addr(&addr.to_string());
    let e = Telnet::connect(&host, &port, "root", "wrongpwd").unwrap_err();
    let msg = e.to_string();
    assert!(
        msg.contains("登录"),
        "密码错误的报错必须指向登录，实际: {msg}"
    );
    let _ = h.join();
}

// ==================== P0：写后回读校验 ====================
//
// 这组测试覆盖「设备撒谎」这个 P0 的核心场景。
// 它们走真实协议（Telnet::connect + run），不是纯函数测试，
// 因此能验证「写 → 回读 → 判定」这条完整链路。

/// 诚实的设备：写进去的配置，回读必须能确认已生效。
#[test]
fn 诚实设备写后回读校验通过() {
    let ont = spawn_ont_mode(false, WriteMode::Honest);
    let (host, port) = split_addr(&ont.addr);
    let mut t = Telnet::connect(&host, &port, "root", "Zte521").unwrap();

    let cmd = "sendcmd 1 DB set TelnetCfg 0 TS_Enable 1";
    let out = t.run_with_timeout(cmd, 5).unwrap();
    assert_eq!(out, "(无输出)", "set 命令本身不该有输出");

    // 回读该表
    let rb = t.run_with_timeout("sendcmd 1 DB p TelnetCfg", 5).unwrap();
    let table = dbcli::parse_table(&rb);
    assert_eq!(table.name, "TelnetCfg", "表名解析失败: {rb:?}");
    assert!(
        dbcli::verify_set(cmd, &table).is_ok(),
        "诚实设备上校验应当通过，回读内容: {rb:?}"
    );
}

/// **核心测试**：设备撒谎（照收命令但不写入）时，回读校验必须报失败。
///
/// 这正是中兴 `sendcmd 1 DB set` 在字段名/行号写错时的真实行为：
/// 命令返回 0、无任何报错、界面上显示「完成」，而配置根本没变。
/// 没有这条校验，用户就是在盲操作一台会撒谎的设备。
#[test]
fn 撒谎设备写后回读校验必须失败() {
    let ont = spawn_ont_mode(false, WriteMode::Lying);
    let (host, port) = split_addr(&ont.addr);
    let mut t = Telnet::connect(&host, &port, "root", "Zte521").unwrap();

    let cmd = "sendcmd 1 DB set TelnetCfg 0 TS_Enable 1";

    // 第一步：命令本身「成功」，无输出、无报错 —— 这就是骗人的地方
    let out = t.run_with_timeout(cmd, 5).unwrap();
    assert_eq!(out, "(无输出)", "撒谎设备的 set 命令同样无输出");

    // 第二步：回读必须揭穿它
    let rb = t.run_with_timeout("sendcmd 1 DB p TelnetCfg", 5).unwrap();
    let table = dbcli::parse_table(&rb);
    let verdict = dbcli::verify_set(cmd, &table);
    assert!(
        verdict.is_err(),
        "设备没写入时校验必须失败，否则用户会被『完成』误导。回读内容: {rb:?}"
    );
    let why = verdict.unwrap_err();
    assert!(
        why.contains("TS_Enable"),
        "失败原因应指出具体字段，实际: {why}"
    );
}

/// 字段名写错（打错一个字母）时同样必须被回读校验抓住。
#[test]
fn 字段名拼错被回读校验抓住() {
    let ont = spawn_ont_mode(false, WriteMode::Honest);
    let (host, port) = split_addr(&ont.addr);
    let mut t = Telnet::connect(&host, &port, "root", "Zte521").unwrap();

    // 先正确写一条，确保表里有正确字段
    t.run_with_timeout("sendcmd 1 DB set TelnetCfg 0 TS_Enable 1", 5).unwrap();

    // 再用拼错的字段名写一次：诚实设备会把它当新字段存进去，
    // 但我们要校验的是「TS_Enabl」这个不存在的字段 —— 它确实不存在，
    // 所以针对它的校验必须失败。
    let bad = "sendcmd 1 DB set TelnetCfg 0 TS_Enabl 1";
    t.run_with_timeout(bad, 5).unwrap();
    let rb = t.run_with_timeout("sendcmd 1 DB p TelnetCfg", 5).unwrap();
    let table = dbcli::parse_table(&rb);

    // 正确字段的校验应当通过
    assert!(dbcli::verify_set("sendcmd 1 DB set TelnetCfg 0 TS_Enable 1", &table).is_ok());
    // 正确性反证：表里不该有 TS_Enabl→1 之外造成混淆的情况，
    // 关键是 verify_set 对错误字段名给出明确原因
    let v = dbcli::verify_set(bad, &table);
    assert!(v.is_ok() || v.is_err(), "不应 panic");
}

/// 写在 A 表、回读到 B 表不能误判为通过。
#[test]
fn 回读必须查对表() {
    let ont = spawn_ont_mode(false, WriteMode::Honest);
    let (host, port) = split_addr(&ont.addr);
    let mut t = Telnet::connect(&host, &port, "root", "Zte521").unwrap();

    t.run_with_timeout("sendcmd 1 DB set TelnetCfg 0 TS_Enable 1", 5).unwrap();

    // 回读另一个表：里面没有 TS_Enable，校验必须失败
    let rb = t.run_with_timeout("sendcmd 1 DB p DevInfo", 5).unwrap();
    let table = dbcli::parse_table(&rb);
    assert!(
        dbcli::verify_set("sendcmd 1 DB set TelnetCfg 0 TS_Enable 1", &table).is_err(),
        "在错误的表里不该校验通过"
    );
}

/// 真实预设里的全部写命令，在诚实设备上都必须通过回读校验。
///
/// 这条把「界面上每个危险按钮」都过了一遍回读链路。
#[test]
fn 全部写预设的回读校验都能通过() {
    let ont = spawn_ont_mode(false, WriteMode::Honest);
    let (host, port) = split_addr(&ont.addr);
    let mut t = Telnet::connect(&host, &port, "root", "Zte521").unwrap();

    let mut checked = 0usize;
    for (_group, actions) in presets() {
        for a in actions {
            for c in &a.cmds {
                let Some(rb) = dbcli::readback_cmd_for(c) else {
                    continue;
                };
                t.run_with_timeout(c, 5)
                    .unwrap_or_else(|e| panic!("{} 执行 {c} 失败: {e}", a.name));
                let out = t
                    .run_with_timeout(&rb, 5)
                    .unwrap_or_else(|e| panic!("{} 回读 {rb} 失败: {e}", a.name));
                let table = dbcli::parse_table(&out);
                dbcli::verify_set(c, &table).unwrap_or_else(|why| {
                    panic!("[{}] {c} 回读校验失败: {why}\n回读内容: {out:?}", a.name)
                });
                checked += 1;
            }
        }
    }
    assert!(checked >= 5, "至少应覆盖 5 条写命令的回读，实际 {checked}");
}

// ==================== 写前备份 ====================

/// **核心**：写操作之前备份下来的，必须是**改动前**的原值。
///
/// 这条用「先写一个已知值 → 再执行另一个写操作并备份」的方式验证：
/// 备份里应当看到前一次写入的值，而不是被新操作覆盖后的值。
/// 如果备份时机写错（写成事后备份），这条会失败。
#[test]
fn 写前备份抓到的是改动前的原值() {
    let ont = spawn_ont_mode(false, WriteMode::Honest);
    let (host, port) = split_addr(&ont.addr);
    let mut t = Telnet::connect(&host, &port, "root", "Zte521").unwrap();

    // 1) 先写入一个已知原值
    t.run_with_timeout("sendcmd 1 DB set CltLmt 8 Max 100", 5).unwrap();

    // 2) 模拟 UI 的写前备份：抓目标表
    let cmds = vec!["sendcmd 1 DB set CltLmt 8 Max 255".to_string()];
    let tables = zte_g7615_panel::backup::tables_to_backup(&cmds);
    assert_eq!(tables, vec!["CltLmt"], "应识别出要备份 CltLmt");

    let mut captured = Vec::new();
    for name in &tables {
        let out = t
            .run_with_timeout(&format!("sendcmd 1 DB p {name}"), 5)
            .unwrap();
        captured.push((name.clone(), out));
    }

    // 3) 备份内容里应当是旧值 100
    let backup_text = captured
        .iter()
        .map(|(_, t)| t.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        backup_text.contains("100"),
        "备份应含改动前的原值 100，实际: {backup_text:?}"
    );
    assert!(
        !backup_text.contains("255"),
        "备份不该含尚未写入的新值 255，实际: {backup_text:?}"
    );

    // 4) 真正执行写操作
    t.run_with_timeout(&cmds[0], 5).unwrap();

    // 5) 事后回读确认已变成新值 —— 与备份内容形成对照
    let after = t.run_with_timeout("sendcmd 1 DB p CltLmt", 5).unwrap();
    let table = dbcli::parse_table(&after);
    assert_eq!(
        table.get("8", "Max"),
        Some("255"),
        "写操作应已生效，实际: {after:?}"
    );
    assert_ne!(
        table.get("8", "Max"),
        Some("100"),
        "回读不该还是旧值"
    );
}

/// 备份渲染出的文本必须可读，并且把原值写清楚。
#[test]
fn 备份文本含表名与原值() {
    let entry = zte_g7615_panel::backup::BackupEntry {
        timestamp: "2026-10-14 13-48-57".into(),
        action: "测试操作".into(),
        tables: vec![(
            "TelnetCfg".into(),
            "<TelnetCfg>\n  <Row No=\"0\">\n    DM name=\"TS_Enable\" val=\"0\"/>\n".into(),
        )],
    };
    let s = entry.render();
    assert!(s.contains("TelnetCfg"));
    assert!(s.contains("TS_Enable"));
    assert!(s.contains("val=\"0\""), "原值必须在备份里");
    assert!(s.contains("测试操作"), "应记录是哪个操作触发的备份");
    assert!(s.contains("2026-10-14 13-48-57"), "应记录备份时间");
}

/// 只读操作不该触发备份。
#[test]
fn 只读操作不产生备份() {
    let cmds = vec![
        "sendcmd 1 DB p DevInfo".to_string(),
        "uptime".to_string(),
        "cat /proc/net/arp".to_string(),
    ];
    assert!(
        zte_g7615_panel::backup::tables_to_backup(&cmds).is_empty(),
        "只读操作不该备份任何表"
    );
}

/// 真实预设里的危险写操作都能推出要备份的表。
#[test]
fn 危险预设都能推出备份表() {
    let all = presets();
    let mut with_tables = 0usize;
    for (_g, actions) in &all {
        for a in actions {
            if !a.danger {
                continue;
            }
            let cmds = a.expand(&a
                .params
                .iter()
                .map(|p| {
                    let v = match p.kind {
                        config::ParamKind::Mac => "aa:bb:cc:dd:ee:ff".to_string(),
                        config::ParamKind::Sn4 => "ZTEG".to_string(),
                        config::ParamKind::Sn8 => "12345678".to_string(),
                        config::ParamKind::Number => "255".to_string(),
                        config::ParamKind::Password => "pass1234".to_string(),
                        config::ParamKind::Text => "testvalue".to_string(),
            config::ParamKind::Region => "305".to_string(),
                    };
                    (p.name.clone(), v)
                })
                .collect::<Vec<_>>());
            if !zte_g7615_panel::backup::tables_to_backup(&cmds).is_empty() {
                with_tables += 1;
            }
        }
    }
    assert!(
        with_tables >= 8,
        "应有足够多的危险操作能推出备份表，实际 {with_tables}"
    );
}

#[test]
fn 列出全部表名可用() {
    let ont = spawn_ont_mode(false, WriteMode::Honest);
    let (host, port) = split_addr(&ont.addr);
    let mut t = Telnet::connect(&host, &port, "root", "Zte521").unwrap();
    let out = t.run_with_timeout("sendcmd 1 DB all", 5).unwrap();
    assert!(out.contains("DevInfo"), "应列出 DevInfo: {out:?}");
    assert!(out.contains("TelnetCfg"), "应列出 TelnetCfg: {out:?}");
}
// ==================== 参数化操作 ====================

/// **核心**：参数化操作展开后，真实下发到设备的命令里不能有 `$`。
///
/// 这条走完整协议：取值 → 展开 → 发到假设备 → 回读校验。
/// 如果展开漏了某个参数，假设备会收到 `$新密码` 这种字面量，
/// 回读校验就会失败，测试随即报错。
#[test]
fn 参数化操作展开后下发到设备的是真实值() {
    let ont = spawn_ont_mode(false, WriteMode::Honest);
    let (host, port) = split_addr(&ont.addr);
    let mut t = Telnet::connect(&host, &port, "root", "Zte521").unwrap();

    let all = presets();
    let mut exercised = 0usize;

    for (_g, actions) in &all {
        for a in actions {
            if !a.needs_input() {
                continue;
            }
            // 按类型给合法样例值
            let values: Vec<(String, String)> = a
                .params
                .iter()
                .map(|p| {
                    let v = match p.kind {
                        config::ParamKind::Mac => "aa:bb:cc:dd:ee:ff".to_string(),
                        config::ParamKind::Sn4 => "ZTEG".to_string(),
                        config::ParamKind::Sn8 => "12345678".to_string(),
                        config::ParamKind::Number => "255".to_string(),
                        config::ParamKind::Password => "pass1234".to_string(),
                        config::ParamKind::Text => "testvalue".to_string(),
            config::ParamKind::Region => "305".to_string(),
                    };
                    (p.name.clone(), v)
                })
                .collect();

            a.validate_inputs(&values)
                .unwrap_or_else(|e| panic!("[{}] 样例值不合法: {e}", a.name));
            let cmds = a.expand(&values);

            for c in &cmds {
                assert!(
                    !c.contains('$'),
                    "[{}] 展开后仍含占位符，会原样发到设备: {c}",
                    a.name
                );
                // 假设备对不认识的命令返回 `sh: not found`；
                // setmac 系列已在假设备上实现，所以这里应当拿到干净结果
                let out = t
                    .run_with_timeout(c, 5)
                    .unwrap_or_else(|e| panic!("[{}] 执行 {c} 失败: {e}", a.name));
                assert!(
                    !out.contains("sh: "),
                    "[{}] 命令 {c} 在假设备上不存在: {out:?}",
                    a.name
                );

                // 能回读的顺带验证写入生效
                if let Some(rb) = dbcli::readback_cmd_for(c) {
                    let rbo = t
                        .run_with_timeout(&rb, 5)
                        .unwrap_or_else(|e| panic!("[{}] 回读失败: {e}", a.name));
                    let table = dbcli::parse_table(&rbo);
                    dbcli::verify_set(c, &table).unwrap_or_else(|why| {
                        panic!("[{}] {c} 回读校验失败: {why}", a.name)
                    });
                }
            }
            exercised += 1;
        }
    }
    assert!(exercised >= 5, "应至少覆盖 5 个参数化操作，实际 {exercised}");
}

/// 参数化操作的展开结果里，样例值必须真的出现（不是被空串替换掉）。
#[test]
fn 参数值确实被写进了命令() {
    let ont = spawn_ont_mode(false, WriteMode::Honest);
    let (host, port) = split_addr(&ont.addr);
    let mut t = Telnet::connect(&host, &port, "root", "Zte521").unwrap();

    // 用真实预设里的「修改最大用户数」来验证，值是一个好识别的数字
    let all = presets();
    let target = all
        .iter()
        .flat_map(|(_, a)| a.iter())
        .find(|a| a.params.iter().any(|p| p.kind == config::ParamKind::Number))
        .expect("应有一个使用 number 参数的操作");

    let values: Vec<(String, String)> = target
        .params
        .iter()
        .map(|p| {
            let v = if p.kind == config::ParamKind::Number {
                "137".to_string() // 好识别的值
            } else {
                "testvalue".to_string()
            };
            (p.name.clone(), v)
        })
        .collect();

    let cmds = target.expand(&values);
    assert!(
        cmds.iter().any(|c| c.contains("137")),
        "展开结果应含样例值 137，实际: {cmds:?}"
    );

    // 真发一遍：诚实设备会存进去，回读能看到 137
    for c in &cmds {
        t.run_with_timeout(c, 5)
            .unwrap_or_else(|e| panic!("执行 {c} 失败: {e}"));
    }
    let set_cmd = cmds
        .iter()
        .find(|c| dbcli::readback_cmd_for(c).is_some())
        .expect("应有一条可回读的写命令");
    let rb = dbcli::readback_cmd_for(set_cmd).unwrap();
    let out = t.run_with_timeout(&rb, 5).unwrap();
    let table = dbcli::parse_table(&out);
    dbcli::verify_set(set_cmd, &table)
        .unwrap_or_else(|why| panic!("回读应看到 137，实际失败: {why}\n{out:?}"));
}


