// Telnet 客户端：仅用标准库 TcpStream 手写协议协商，兼容中兴光猫 busybox。
use std::fmt;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const IAC: u8 = 255;
const DONT: u8 = 254;
const DO: u8 = 253;
const WONT: u8 = 252;
const WILL: u8 = 251;
const SB: u8 = 250;
const SE: u8 = 240;
const EOR: u8 = 239;
const GA: u8 = 249;
const EL: u8 = 248;
const EC: u8 = 247;
const AYT: u8 = 246;
const AO: u8 = 245;
const IP: u8 = 244;
const BRK: u8 = 243;
const DM: u8 = 242;
const NOP: u8 = 241;

/// 不带选项字节的命令：IAC 后面跟 1 个字节就结束，共 2 字节。
///
/// 含 `SE`(240)：裸的 `IAC SE` 也是两字节，若按三字节处理会吞掉正文首字节。
/// 未分配的保留值（236–238 等）同样按两字节保守处理，见 `parse_iac`。
const TWO_BYTE_CMDS: [u8; 11] = [NOP, DM, BRK, IP, AO, AYT, EC, EL, GA, EOR, SE];

/// 单条命令的等待上限（秒）。`sendcmd 1 DB save` 这类慢命令要留足时间。
const CMD_TIMEOUT_SECS: u64 = 20;
/// 登录各阶段的等待上限（秒）。
const LOGIN_TIMEOUT_SECS: u64 = 10;
/// 超时后重新对齐会话的等待上限（秒）。
const RESYNC_TIMEOUT_SECS: u64 = 15;
/// 重新对齐时判定「已安静」所需的静默时长（毫秒）。
const RESYNC_IDLE_MS: u64 = 700;
/// 重新对齐最多进行几轮，避免设备持续刷输出时无限循环。
const RESYNC_MAX_ROUNDS: usize = 5;
/// 登录成功后 shell 派生的等待上限（秒）。光猫冷启动会比较慢。
const LOGIN_VERIFY_TIMEOUT_SECS: u64 = 15;
/// 读写 socket 的阻塞上限（秒）。设备假死不读时不让 worker 永久卡住。
const IO_TIMEOUT_SECS: u64 = 5;
/// 未闭合的 IAC 子协商最多缓存多少字节，防止设备只发 SB 不发 SE 时无限堆积。
const IAC_PENDING_LIMIT: usize = 64 * 1024;

/// 登录后由我们自己 export 的 shell 提示符。
/// shell 打印它时**不带换行**，所以它会把下一条命令的输出/结束标记粘在自己后面；
/// 解析时必须把这个前缀剥掉（见 `strip_prompt`）。
const PROMPT: &str = "G7615> ";

/// 错误分类：上层据此决定连接对象是否还能继续用。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    /// 连接断开 / 读写失败，该连接对象必须丢弃
    Disconnected,
    /// 连接还在，但命令在超时时间内没给出结束标记
    Timeout,
}

#[derive(Debug)]
pub struct TelnetError {
    pub kind: ErrKind,
    pub msg: String,
}

impl fmt::Display for TelnetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl TelnetError {
    fn disc(msg: impl Into<String>) -> Self {
        Self { kind: ErrKind::Disconnected, msg: msg.into() }
    }
    fn timeout(msg: impl Into<String>) -> Self {
        Self { kind: ErrKind::Timeout, msg: msg.into() }
    }
}

/// 只在含 telnet 元数据的调试信息里用到；`Debug` 不输出密码等敏感内容。
#[derive(Debug)]
pub struct Telnet {
    stream: TcpStream,
    /// 跨 TCP 分包保存未解析完的 IAC 序列
    iac: IacFilter,
}

/// 结束标记/同步标记用的唯一串。
///
/// 只靠墙钟纳秒是不够的：Windows 上 `SystemTime::now()` 底层是
/// `GetSystemTimeAsFileTime`，分辨率约 15.6 ms，LAN 上两条快命令的往返
/// 完全可以落在同一个 tick 里，导致两次取到同一个值。前一条命令超时后
/// 迟到的标记会被下一条当成命中。所以再并上一个进程内自增序号。
fn nonce() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{}", nanos, seq)
}

/// 判断缓冲区里是否出现「整行等于 target」的行。
/// 结束标记必须这样判：设备会回显命令本身，而回显里同样含有标记字符串，
/// 用子串匹配会在回显刚到达时就误判命令已结束。
fn has_whole_line(text: &str, targets: &[&str]) -> bool {
    text.split('\n').any(|l| targets.iter().any(|t| marker_matches(l, t)))
}

/// 单行是否算命中结束标记。
///
/// 先剥掉行首可能残留的提示符再比较：上一条命令留下的 `G7615> ` 不带换行，
/// 会把标记粘成 `G7615> E123`。关闭回显时尤其常见；若只认整行相等，
/// 空输出命令（`sendcmd 1 DB set/save`）会永远等不到标记、干等满超时。
///
/// 注意仍然要求「整行等于标记」——不能退化成子串匹配，
/// 否则命令回显那一行（含同一串标记）也会被误判成命令已结束。
fn marker_matches(line: &str, marker: &str) -> bool {
    strip_prompt(line.trim_end()) == marker
}

/// shell 提示符前缀的最大长度（超过就不当提示符处理，避免误判正文）。
const PROMPT_MAX_LEN: usize = 64;

/// 剥掉行首残留的 shell 提示符。
///
/// 关闭回显时，上一条命令留下的提示符（不带换行）会粘在真实输出首行，
/// 例如 `G7615>  12:00:00 up 100 days`。不剥掉就会漏进输出。
///
/// **关键约束**：绝不能把「正文里以 > 结尾的行」误当成提示符。
/// `sendcmd 1 DB p DevInfo` 的输出首行就是 `<DevInfo>`，
/// 若按「找第一个 > 就切」来处理，整行内容都会被吃掉。
/// 所以这里要求提示符**后面必须跟一个空格**（行尾的 > 不满足），
/// 并且优先精确匹配我们自己设置的那个提示符。
///
/// 只吃掉提示符号和紧跟其后的**一个**分隔空格，
/// 保留输出原有的缩进——`uptime`/`sendcmd` 表格靠对齐才能读。
fn strip_prompt(line: &str) -> &str {
    // 1) 优先精确匹配我们自己 export 的 PS1（最可靠）
    if let Some(rest) = line.strip_prefix(PROMPT) {
        return rest;
    }
    // 2) 退路：识别「短 token + >/#/$ + 空格」的一般提示符
    //    （设备可能忽略 export PS1，提示符就不是 G7615> 了）
    for (i, c) in line.char_indices() {
        if c == '>' || c == '#' || c == '$' {
            let head = &line[..i];
            let tail = &line[i + c.len_utf8()..];
            // 提示符后必须是空格：这样 `<DevInfo>`（> 在行尾）不会被误吃
            if !tail.starts_with(' ') {
                return line;
            }
            // 头部要像个提示符：非空、够短、且不以空格结尾（排除 "a > b" 这类正文）
            if head.is_empty() || head.len() > PROMPT_MAX_LEN || head.ends_with(' ') {
                return line;
            }
            return &tail[1..];
        }
    }
    line
}

impl Telnet {
    pub fn connect(host: &str, port: &str, user: &str, pwd: &str) -> Result<Self, TelnetError> {
        let host = host.trim();
        let port = port.trim();
        // 端口先按数字校验，给出比「地址无法解析」更明确的提示
        if port.parse::<u16>().is_err() {
            return Err(TelnetError::disc(format!("端口不合法: {}（应填数字，Telnet 默认是 23）", port)));
        }
        // 用 to_socket_addrs 而不是 parse::<SocketAddr>()：
        // 后者只认 IP 字面量，会把主机名（如 localhost）误报成「地址无法解析」。
        // 主机名可能解析出多个地址（例如同时有 ::1 和 127.0.0.1），
        // 要依次尝试，不能只试第一个——否则只监听 IPv4 的设备会被判成连不上。
        let port_num = port.parse::<u16>().map_err(|_| {
            TelnetError::disc(format!("端口不合法: {port}（应填数字，Telnet 默认是 23）"))
        })?;
        let addrs: Vec<_> = (host, port_num)
            .to_socket_addrs()
            .map_err(|_| TelnetError::disc(format!("地址无法解析: {host}:{port}（请检查 IP/主机名）")))?
            .collect();
        if addrs.is_empty() {
            return Err(TelnetError::disc(format!("地址无法解析: {host}:{port}")));
        }
        let mut stream = None;
        let mut last_err = None;
        for addr in &addrs {
            match TcpStream::connect_timeout(addr, Duration::from_secs(8)) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let stream = match stream {
            Some(s) => s,
            None => {
                let e = last_err
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "所有解析出的地址都连不上".into());
                return Err(TelnetError::disc(format!("连接失败: {e}")));
            }
        };
        // telnet 是交互式小包，禁用 Nagle 避免命令回显延迟
        let _ = stream.set_nodelay(true);
        stream
            .set_read_timeout(Some(Duration::from_millis(300)))
            .map_err(|e| TelnetError::disc(e.to_string()))?;
        // 写也要设上限：设备假死不读时不让 worker 线程永久卡住
        let _ = stream.set_write_timeout(Some(Duration::from_secs(IO_TIMEOUT_SECS)));

        let mut t = Telnet { stream, iac: IacFilter::default() };

        // 1) 等登录提示符。没等到就说明对方根本不是 telnet 服务，
        //    此时继续发账号会被当成 shell 命令，整条会话都会错位。
        t.read_until(&["ogin:"], LOGIN_TIMEOUT_SECS)?.ok_or_else(|| {
            TelnetError::disc("没有收到登录提示，请确认设备已开启 Telnet 且端口为 23（不是 8080）")
        })?;

        // 2) 发账号，等密码提示符
        t.send_line(user)?;
        t.read_until(&["assword:"], LOGIN_TIMEOUT_SECS)?.ok_or_else(|| {
            TelnetError::disc("没有收到密码提示，账号可能已被拒绝")
        })?;

        // 3) 发密码，等 shell 就绪
        t.send_line(pwd)?;
        let after = t.settle(1.2);

        t.send_line("export PS1='G7615> '")?;
        let _ = t.settle(0.6);

        // 4) 用唯一标记验证登录真的成功。
        //    标记必须整行匹配：回显里的同一串字符不算数，否则密码错误也会「通过」。
        let tag = format!("ZTE{}", nonce());
        t.send_line(&format!("echo {}", tag))?;
        return match t.read_marker(&tag, LOGIN_VERIFY_TIMEOUT_SECS) {
            Ok(Some(_)) => Ok(t),
            Ok(None) => Err(login_fail_error(&t.diagnose_text(&after), false)),
            // 验证途中连接断了：这同样属于「没能确认登录成功」。
            // 设备常常在密码错误后直接踢掉连接，若退回成笼统的
            // 「与光猫的连接已断开」，用户会去查网络而不是查密码。
            Err(_) => Err(login_fail_error(&t.diagnose_text(&after), true)),
        };
    }

    /// 执行一条命令，返回去掉回显/提示符/结束标记后的文本。
    pub fn run(&mut self, cmd: &str) -> Result<String, TelnetError> {
        self.run_with_timeout(cmd, CMD_TIMEOUT_SECS)
    }

    /// 同 `run`，但可指定等待结束标记的秒数。
    ///
    /// 任何一次失败都会把连接恢复到「干净」状态：断链直接返回错误；
    /// 超时会重新对齐会话（发一条同步命令并读到它的标记），
    /// 让下一条命令不会把上一条迟到的输出当成自己的结果。
    pub fn run_with_timeout(&mut self, cmd: &str, timeout_secs: u64) -> Result<String, TelnetError> {
        let marker = format!("E{}", nonce());
        self.send_line(&format!("{} ; echo {}", cmd, marker))?;
        let data = match self.read_marker(&marker, timeout_secs)? {
            Some(d) => d,
            None => {
                // 超时：命令可能还在跑，残留输出还没到齐。
                // 用一条独立的同步命令把会话重新对齐，而不是只靠 drain 猜时间窗。
                let lost = self.resync();
                if lost {
                    // 同步途中对端关闭：说明连接真的断了，不该报成超时
                    return Err(TelnetError::disc(
                        "命令执行中对端关闭了连接（设备重启或断链）",
                    ));
                }
                return Err(TelnetError::timeout(format!(
                    "命令 {} 秒内没有返回（仍在执行？设备已无响应？），残留输出已丢弃",
                    timeout_secs
                )));
            }
        };

        let text = String::from_utf8_lossy(&data).replace('\r', "");
        // 结束标记之后就是下一个命令的提示符，直接丢弃
        let mut lines: Vec<&str> = Vec::new();
        for line in text.split('\n') {
            if marker_matches(line, &marker) {
                break;
            }
            lines.push(line);
        }
        // 去掉首行命令回显（回显里含 marker，所以不会被上面的整行判断吃掉）
        let needle = cmd.trim();
        if let Some(first) = lines.first() {
            if !needle.is_empty() && first.contains(needle) {
                lines.remove(0);
            }
        }
        // 剥掉残留提示符前缀。
        // 关闭回显时，上一条命令留下的提示符（不带换行）会粘在
        // 真实输出首行前面，必须清掉，否则提示符会漏进输出。
        if let Some(first) = lines.first_mut() {
            *first = strip_prompt(first);
        }
        // 只清掉首尾空行/空白，保留正文里的缩进：
        // uptime、sendcmd 表格这类输出靠对齐才能读
        let out = lines.join("\n").trim_end().trim_start_matches('\n').to_string();
        Ok(if out.is_empty() { "(无输出)".into() } else { out })
    }

    /// 限时内把会话重新对齐到「shell 空闲等下一条命令」的状态。
    ///
    /// 单纯读一段时间（`drain`）不可靠：迟到的输出完全可能在这个窗口之后才到，
    /// 于是会被下一条命令当成自己的结果。这里改成「同步命令 + 静默观察」的循环：
    /// 每轮发一条 `echo S<token>` 并等它的标记，标记之前的一律算陈旧输出丢弃；
    /// 标记之后还要观察一段静默期，若期间又冒出数据（说明还有迟到输出），
    /// 就再同步一轮，直到某一轮全程安静为止。
    ///
    /// 返回 true 表示同步途中发现连接已断开。
    fn resync(&mut self) -> bool {
        for _ in 0..RESYNC_MAX_ROUNDS {
            let token = format!("S{}", nonce());
            if self.send_line(&format!("echo {}", token)).is_err() {
                return true;
            }
            match self.read_marker(&token, RESYNC_TIMEOUT_SECS) {
                Ok(Some(_)) => {
                    // 标记已到；再看之后是否还有迟到输出
                    match self.quiet(RESYNC_IDLE_MS) {
                        Ok(true) => return false, // 全程安静，会话已对齐
                        Ok(false) => continue,    // 还有迟到数据，再来一轮
                        Err(_) => return true,    // 观察期间连接断了
                    }
                }
                // 没等到标记：可能是设备真的卡死，也可能连接已断
                Ok(None) => return false,
                Err(_) => return true,
            }
        }
        // 轮次用尽仍未安静：尽力而为，交给下一条命令自己判定
        false
    }

    /// 观察 socket 是否安静：持续 `idle_ms` 毫秒没有数据返回 true。
    /// 期间一旦收到数据就重置静默计时并继续观察（有整体硬上限）。
    /// 返回 false 表示「不安静」（期间收到过数据）。
    fn quiet(&mut self, idle_ms: u64) -> Result<bool, TelnetError> {
        let idle = Duration::from_millis(idle_ms);
        let hard_stop = Instant::now() + Duration::from_secs(RESYNC_TIMEOUT_SECS);
        let mut deadline = Instant::now() + idle;
        let mut saw_data = false;
        loop {
            let chunk = self.recv()?;
            if !chunk.is_empty() {
                saw_data = true;
                deadline = Instant::now() + idle;
            }
            if Instant::now() >= deadline {
                return Ok(!saw_data);
            }
            if Instant::now() >= hard_stop {
                return Ok(!saw_data);
            }
        }
    }

    /// 把已收到的内容和再读一小段的内容拼起来，只用于登录失败的文案诊断。
    fn diagnose_text(&mut self, first: &[u8]) -> String {
        let extra = self.settle(0.5);
        String::from_utf8_lossy(first)
            .chars()
            .chain(String::from_utf8_lossy(&extra).chars())
            .collect::<String>()
            .to_lowercase()
    }

    fn send_line(&mut self, s: &str) -> Result<(), TelnetError> {
        self.stream
            .write_all(format!("{}\r\n", s).as_bytes())
            .map_err(|e| TelnetError::disc(format!("发送失败: {}", e)))?;
        self.stream.flush().map_err(|e| TelnetError::disc(format!("发送失败: {}", e)))
    }

    /// 读一块数据。读超时/无数据返回空 Vec（由调用方的截止时间控制节奏），
    /// 对端关闭返回 Disconnected，其他 IO 错误同样视为断链。
    fn recv(&mut self) -> Result<Vec<u8>, TelnetError> {
        let mut buf = [0u8; 4096];
        match self.stream.read(&mut buf) {
            Ok(0) => Err(TelnetError::disc("与光猫的连接已断开")),
            Ok(n) => {
                let (clean, replies) = self.iac.feed(&buf[..n]);
                for r in replies {
                    // 协商应答发不出去不影响正文，忽略
                    let _ = self.stream.write_all(&r);
                }
                Ok(clean)
            }
            Err(ref e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) =>
            {
                Ok(vec![])
            }
            Err(e) => Err(TelnetError::disc(format!("网络错误: {}", e))),
        }
    }

    /// 读取直到 `hit` 判定命中为止；到截止时间仍未命中返回 Ok(None)。
    ///
    /// `acc` 会一直累计，但 `hit` 只扫描**新增的尾部**（`checked` 之前的部分
    /// 上一轮已经判过了）。否则每收到一个 chunk 就要把整块缓冲重新
    /// `from_utf8_lossy` + 逐行扫描，大输出时是 O(n²)。
    fn read_matching<F>(&mut self, timeout: u64, mut hit: F) -> Result<Option<Vec<u8>>, TelnetError>
    where
        F: FnMut(&str) -> bool,
    {
        let mut acc: Vec<u8> = Vec::new();
        // 已判定过的位置。回退一个字符以防多字节字符被拆在两个 chunk 之间。
        let mut checked: usize = 0;
        let deadline = Instant::now() + Duration::from_secs(timeout);
        loop {
            // recv 最多阻塞 300ms，超时误差有上限
            let chunk = self.recv()?;
            if !chunk.is_empty() {
                let start = checked;
                acc.extend_from_slice(&chunk);
                // 从上一轮已判定位置（回退一点，容忍拆分的多字节字符）开始
                let from = start.saturating_sub(8);
                // 对齐到 UTF-8 边界：from 可能落在多字节字符中间
                let from = (from..acc.len().min(from + 8))
                    .find(|&k| std::str::from_utf8(&acc[k..]).is_ok())
                    .unwrap_or(from);
                if hit(&String::from_utf8_lossy(&acc[from..])) {
                    return Ok(Some(acc));
                }
                // 只把「整行已确定」的部分标记为已判定：
                // 保留最后一行，因为它可能还没读完
                if let Some(p) = acc.iter().rposition(|&x| x == b'\n') {
                    checked = p + 1;
                }
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
        }
    }

    /// 等提示符（子串匹配：提示符可能和 banner 同行）。
    fn read_until(&mut self, targets: &[&str], timeout: u64) -> Result<Option<Vec<u8>>, TelnetError> {
        self.read_matching(timeout, |t: &str| targets.iter().any(|x| t.contains(x)))
    }

    /// 等结束标记（整行匹配，避免被命令回显误命中）。
    fn read_marker(&mut self, marker: &str, timeout: u64) -> Result<Option<Vec<u8>>, TelnetError> {
        self.read_matching(timeout, |t: &str| has_whole_line(t, &[marker]))
    }

    /// 读 secs 秒并把内容收下返回（登录阶段用来确认 shell 是否就绪）。
    fn settle(&mut self, secs: f32) -> Vec<u8> {
        let mut acc: Vec<u8> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs_f32(secs);
        loop {
            let chunk = match self.recv() {
                Ok(c) => c,
                Err(_) => break,
            };
            if chunk.is_empty() {
                if Instant::now() >= deadline {
                    break;
                }
            } else {
                acc.extend_from_slice(&chunk);
            }
        }
        acc
    }

}

/// 登录被拒绝时设备常见的提示关键词（小写匹配）。
fn reject_reason(text_lowercase: &str) -> Option<&'static str> {
    // 注意顺序：更具体的词放前面，避免笼统的 "failed" 抢先命中
    [
        "login incorrect",
        "invalid password",
        "密码错误",
        "denied",
        "incorrect",
        "failed",
    ]
    .into_iter()
    .find(|k| text_lowercase.contains(k))
}

/// 登录没被确认时统一生成错误，保证文案一定指向「登录」，
/// 不会退回到笼统的「连接已断开」（那样用户根本不知道该查什么）。
fn login_fail_error(text_lowercase: &str, disconnected: bool) -> TelnetError {
    if let Some(why) = reject_reason(text_lowercase) {
        TelnetError::disc(format!("登录被拒绝：{}", why))
    } else if text_lowercase.contains("ogin") || text_lowercase.contains("assword") {
        TelnetError::disc("登录未成功：仍停在登录提示符处，请检查账号/密码")
    } else if disconnected {
        TelnetError::disc("登录未成功：验证登录时连接已断开，请确认账号密码是否正确")
    } else {
        TelnetError::disc("登录未成功：登录后没有拿到 shell 响应，请确认账号密码是否正确")
    }
}

/// 保存跨包的未完成 IAC 序列。
#[derive(Default, Debug)]
struct IacFilter {
    pending: Vec<u8>,
}

impl IacFilter {
    /// 返回 (清洗后的正文, 需要回发的协商应答)。
    /// 一条协商指令完全可能跨两个 TCP 包，所以尾部没解析完的部分要留到下一包。
    fn feed(&mut self, data: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>) {
        self.pending.extend_from_slice(data);
        // 设备发了 IAC SB 却迟迟不给 IAC SE 时，pending 会无限囤积正文。
        // 超过上限就丢弃已缓存的部分（这些字节永远等不到配对的 SE 了），
        // 保证内存有界、也不会每轮都重扫一大块。
        if self.pending.len() > IAC_PENDING_LIMIT {
            self.pending.drain(..self.pending.len() - IAC_PENDING_LIMIT);
        }
        let buf = std::mem::take(&mut self.pending);
        let (out, replies, used) = parse_iac(&buf);
        self.pending.extend_from_slice(&buf[used..]);
        (out, replies)
    }
}

/// 剥离 Telnet IAC 协商字节，并生成应答。
/// `used` 是已消费到的位置；`buf[used..]` 是不完整序列，需要等下一包。
fn parse_iac(data: &[u8]) -> (Vec<u8>, Vec<Vec<u8>>, usize) {
    let mut out = Vec::new();
    let mut replies = Vec::new();
    let mut i = 0;
    let n = data.len();
    while i < n {
        let b = data[i];
        if b != IAC {
            out.push(b);
            i += 1;
            continue;
        }
        // 至少要 2 字节才能判断是哪种序列；不足就留到下一包
        let Some(&second) = data.get(i + 1) else {
            return (out, replies, i);
        };
        if second == IAC {
            // 转义：数据里的字面 0xFF
            out.push(IAC);
            i += 2;
            continue;
        }
        if second == SB {
            // 子协商：必须整体跳过到 IAC SE，否则内容会漏进正文
            let mut j = i + 2;
            while j + 1 < n && !(data[j] == IAC && data[j + 1] == SE) {
                j += 1;
            }
            if j + 1 >= n {
                return (out, replies, i); // 子协商尚未结束
            }
            i = j + 2;
            continue;
        }
        if TWO_BYTE_CMDS.contains(&second) {
            i += 2;
            continue;
        }
        // 只有 WILL/WONT/DO/DONT(251-254) 才是「命令 + 选项」共 3 字节。
        // 其余任何值（RFC 未分配的保留值、线路噪声）一律按 2 字节消费：
        // 按 3 字节处理会把紧随其后的正文字节当成选项吞掉，造成静默丢字符。
        let reply_prefix = match second {
            WILL => Some(DONT),
            DO => Some(WONT),
            DONT => Some(WONT),
            WONT => Some(DONT),
            _ => None,
        };
        let Some(prefix) = reply_prefix else {
            i += 2;
            continue;
        };
        if i + 2 >= n {
            return (out, replies, i);
        }
        let opt = data[i + 2];
        replies.push(vec![IAC, prefix, opt]);
        i += 3;
    }
    (out, replies, i)
}

#[cfg(test)]
#[allow(non_snake_case)] // 测试名用中文描述更直观
mod tests {
    use super::*;
    use std::net::TcpListener;

    // ---------- IAC 协商解析 ----------

    #[test]
    fn 子协商必须整体跳过且不吃正文() {
        // 设备发一次子协商（终端类型），后面跟着真正的提示符
        let mut input = vec![IAC, SB, 24, b'I', b'S', 0, 1, 2, IAC, SE];
        input.extend_from_slice(b"login:");
        let (out, replies, used) = parse_iac(&input);
        assert_eq!(String::from_utf8_lossy(&out), "login:");
        assert!(replies.is_empty());
        assert_eq!(used, input.len());
    }

    #[test]
    fn 子协商被拆包后仍能拼回来() {
        let mut f = IacFilter::default();
        let (o1, r1) = f.feed(&[b'a', IAC, SB, 24, b'X']);
        assert_eq!(o1, b"a");
        assert!(r1.is_empty());
        // 第二包补齐 IAC SE，正文里的 l 不能被吃掉
        let (o2, _) = f.feed(&[b'Y', IAC, SE, b'l', b'o', b'g']);
        assert_eq!(o2, b"log");
        assert!(f.pending.is_empty());
    }

    #[test]
    fn 转义的0xFF按一个字面字节处理() {
        let (out, _, used) = parse_iac(&[IAC, IAC, b'X', b'Y']);
        assert_eq!(out, vec![IAC, b'X', b'Y']);
        assert_eq!(used, 4);
    }

    #[test]
    fn 尾部不完整的序列留给下一包() {
        let (out, _, used) = parse_iac(&[b'o', b'k', IAC]);
        assert_eq!(out, b"ok");
        assert_eq!(used, 2, "末尾裸 IAC 不应进正文");
        let (out2, _, _) = parse_iac(&[IAC, WILL, 1, b'o', b'k']);
        assert_eq!(out2, b"ok");
    }

    #[test]
    fn 两字节命令不吞掉后面的正文() {
        let (out, replies, _) = parse_iac(&[IAC, WILL, 1, IAC, DO, 3, IAC, NOP, b'o', b'k']);
        assert_eq!(out, b"ok");
        assert_eq!(replies, vec![vec![IAC, DONT, 1], vec![IAC, WONT, 3]]);
    }

    // ---------- 结束标记判定 ----------

    /// 回归：`sendcmd 1 DB p` 的输出首行是 `<表名>`（以 > 结尾），
    /// 剥提示符时绝不能把这种正文行当成提示符而整行吃掉。
    ///
    /// 这是端到端测试抓出来的真实回归：早期实现「找第一个 > 就切」，
    /// 会把 `<DevInfo>` 整行吃成空串，导致 `sendcmd 1 DB p` 全部丢首行。
    #[test]
    fn 以尖括号结尾的输出行不能被当成提示符剥掉() {
        assert_eq!(strip_prompt("<DevInfo>"), "<DevInfo>");
        assert_eq!(strip_prompt("<WANC>"), "<WANC>");
        assert_eq!(strip_prompt("<TelnetCfg>"), "<TelnetCfg>");
        assert_eq!(strip_prompt("  id=0  name=test"), "  id=0  name=test");
        // 缩进要保留（uptime / sendcmd 表格靠对齐才能读）
        assert_eq!(strip_prompt("G7615>  12:00:00 up 100"), " 12:00:00 up 100");
        // 我们自己的提示符（含粘住的正文行）必须能剥掉
        assert_eq!(strip_prompt("G7615> <DevInfo>"), "<DevInfo>");
        // 设备自己换过提示符也能识别：提示符后必须跟空格
        assert_eq!(strip_prompt("root@OpenWrt:~# ls"), "ls");
        // 正文里的 > 不该被动（前有空格的头部不像提示符）
        assert_eq!(strip_prompt("a > b"), "a > b");
        // 有意的取舍：光是 "# " 这种「提示符即单个符号」不当作提示符，
        // 否则会把 shell 注释行 `# 这是说明` 之类的正文字节吃掉。
        // 实际运行时走的是上面精确匹配我们 PS1 的那条路径，不受影响。
        assert_eq!(strip_prompt("# ls -l"), "# ls -l");
    }

    /// 回归：粘了提示符的结束标记仍要命中，但回显行不能命中。
    #[test]
    fn 结束标记剥离提示符后仍整行匹配() {
        assert!(marker_matches("G7615> E123", "E123"));
        assert!(marker_matches("E123", "E123"));
        // 回显行里有同一串标记，但不是整行相等 → 绝不能命中
        assert!(!marker_matches("uptime ; echo E123", "E123"));
        // 以 > 结尾的正文行不能被误判
        assert!(!marker_matches("<DevInfo>", "E123"));
        // 提示符 + 正文里恰好含标记，但不是行尾 → 不命中
        assert!(!marker_matches("G7615> E123 extra", "E123"));
    }

    #[test]
    fn 回显行不能被当成结束标记() {
        // 只有回显：不含结束标记，不能判定命令已结束
        assert!(!has_whole_line("uptime ; echo E12345\r\n", &["E12345"]));
        // 回显 + 真正的标记行 + 提示符
        assert!(has_whole_line("uptime ; echo E12345\r\nE12345\r\nG7615> ", &["E12345"]));
        // 结尾没有换行也算命中
        assert!(has_whole_line("E12345", &["E12345"]));
        // 前面有残留内容时，首行不能误判
        assert!(!has_whole_line("some noise\nnot the marker E12345 here", &["E12345"]));
    }

    // ---------- run() 与假设备 ----------

    struct Fake {
        addr: String,
        _h: std::thread::JoinHandle<()>,
    }

    fn spawn_fake<F: FnOnce(TcpStream) + Send + 'static>(f: F) -> Fake {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            f(s);
        });
        Fake { addr, _h: h }
    }

    fn read_line(s: &mut TcpStream) -> String {
        let mut out = Vec::new();
        let mut b = [0u8; 1];
        loop {
            let n = s.read(&mut b).unwrap();
            if n == 0 || b[0] == b'\n' {
                break;
            }
            if b[0] != b'\r' {
                out.push(b[0]);
            }
        }
        String::from_utf8_lossy(&out).to_string()
    }

    /// 跳过登录流程，直接拿到一条已就绪的连接
    fn test_link(addr: &str) -> Telnet {
        let stream = TcpStream::connect(addr).unwrap();
        let _ = stream.set_read_timeout(Some(Duration::from_millis(300)));
        Telnet { stream, iac: IacFilter::default() }
    }

    fn marker_of(cmd_line: &str) -> String {
        cmd_line.split(" ; echo ").nth(1).unwrap().trim().to_string()
    }

    #[test]
    fn 开启回显时也能拿到完整输出() {
        let f = spawn_fake(|mut s| {
            // 上一条命令残留的提示符（不带换行）会粘在这一轮的第一行前面
            s.write_all(b"G7615> ").unwrap();
            let line = read_line(&mut s);
            let marker = marker_of(&line);
            // 回显 → 输出 → 结束标记 → 提示符，分三批发（模拟真实分包）
            s.write_all(format!("{}\r\n", line).as_bytes()).unwrap();
            s.write_all(b" 12:00:00 up 100 days\r\n").unwrap();
            s.write_all(format!("{}\r\nG7615> ", marker).as_bytes()).unwrap();
        });
        let mut t = test_link(&f.addr);
        assert_eq!(t.run("uptime").unwrap(), " 12:00:00 up 100 days");
    }

    #[test]
    fn 关闭回显时同样正常() {
        let f = spawn_fake(|mut s| {
            // 关闭回显：没有回显行，残留提示符直接粘在真实输出前面
            s.write_all(b"G7615> ").unwrap();
            let line = read_line(&mut s);
            let marker = marker_of(&line);
            s.write_all(b" 00:00:00 up 3 days\r\n").unwrap();
            s.write_all(format!("{}\r\nG7615> ", marker).as_bytes()).unwrap();
        });
        let mut t = test_link(&f.addr);
        assert_eq!(t.run("uptime").unwrap(), " 00:00:00 up 3 days");
    }

    #[test]
    fn 空输出显示为无输出() {
        let f = spawn_fake(|mut s| {
            let line = read_line(&mut s);
            let marker = marker_of(&line);
            s.write_all(format!("{}\r\n{}\r\nG7615> ", line, marker).as_bytes()).unwrap();
        });
        let mut t = test_link(&f.addr);
        assert_eq!(t.run("true").unwrap(), "(无输出)");
    }

    /// B1 回归：关闭回显 + 命令无输出时，残留提示符会和结束标记粘成
    /// `G7615> E123` 一行。整行匹配若不剥提示符就永远命中不了，只能干等到超时。
    #[test]
    fn 关闭回显且无输出时结束标记不被提示符粘住() {
        let f = spawn_fake(|mut s| {
            s.write_all(b"G7615> ").unwrap();
            let line = read_line(&mut s);
            let marker = marker_of(&line);
            // 无输出：marker 直接跟在残留提示符后面
            s.write_all(format!("{}\r\nG7615> ", marker).as_bytes()).unwrap();
        });
        let mut t = test_link(&f.addr);
        // 3 秒足够返回；一旦退化成「干等 20 秒再超时」这条就会失败
        assert_eq!(t.run_with_timeout("sendcmd 1 DB save", 3).unwrap(), "(无输出)");
    }

    /// B2 回归：残留提示符粘在真实输出首行时，必须剥掉，不能漏进输出。
    #[test]
    fn 关闭回显时提示符不会漏进输出() {
        let f = spawn_fake(|mut s| {
            s.write_all(b"G7615> ").unwrap();
            let line = read_line(&mut s);
            let marker = marker_of(&line);
            s.write_all(b" 00:00:00 up 3 days\r\n").unwrap();
            s.write_all(format!("{}\r\nG7615> ", marker).as_bytes()).unwrap();
        });
        let mut t = test_link(&f.addr);
        let out = t.run("uptime").unwrap();
        assert!(!out.contains("G7615"), "提示符漏进了输出: {out:?}");
        assert_eq!(out, " 00:00:00 up 3 days");
    }

    /// B5 回归：连续两条命令的结束标记不能重复，
    /// 否则前一条超时的迟到标记会被下一条当成命中。
    #[test]
    fn 连续两条命令的结束标记不重复() {
        let f = spawn_fake(|mut s| {
            let l1 = read_line(&mut s);
            let m1 = marker_of(&l1);
            s.write_all(format!("{}\r\nG7615> ", m1).as_bytes()).unwrap();
            let l2 = read_line(&mut s);
            let m2 = marker_of(&l2);
            s.write_all(format!("{}\r\nG7615> ", m2).as_bytes()).unwrap();
        });
        let mut t = test_link(&f.addr);
        assert_eq!(t.run_with_timeout("cmd1", 3).unwrap(), "(无输出)");
        assert_eq!(t.run_with_timeout("cmd2", 3).unwrap(), "(无输出)");
    }

    /// B4 回归：裸 IAC SE 是两字节命令，不能按三字节吞掉后面的正文。
    #[test]
    fn 裸IAC_SE只占两字节不吃正文() {
        // IAC SE 后面紧跟正文 'X'：若按三字节处理，X 会被当成选项字节吞掉
        let (out, _, used) = parse_iac(&[IAC, SE, b'X', b'Y']);
        assert_eq!(out, b"XY", "IAC SE 之后的正文不能被吃");
        assert_eq!(used, 4);
    }

    /// S1 回归：未知 IAC 命令按两字节消费，不能吞掉后面的正文。
    #[test]
    fn 未知IAC命令只占两字节不吃正文() {
        // 236 是 RFC 未分配的保留值
        let (out, _, used) = parse_iac(&[236, b'X', b'Y']);
        // 236 不是 IAC，应作为普通字节保留
        assert_eq!(out, &[236, b'X', b'Y']);
        assert_eq!(used, 3);

        let (out2, _, used2) = parse_iac(&[IAC, 236, b'X', b'Y']);
        assert_eq!(out2, b"XY", "未知 IAC 命令之后的正文不能被吃");
        assert_eq!(used2, 4);
    }

    /// B8 回归：超时窗口内对端关闭时，应报 Disconnected 而不是 Timeout，
    /// 否则上层不会丢弃连接对象、状态灯也不转红。
    #[test]
    fn 超时窗口内对端关闭按断链处理() {
        let f = spawn_fake(|mut s| {
            let _ = read_line(&mut s);
            std::thread::sleep(Duration::from_millis(300));
            drop(s); // 命令还没返回就断开
        });
        let mut t = test_link(&f.addr);
        let e = t.run_with_timeout("reboot", 2).unwrap_err();
        assert_eq!(e.kind, ErrKind::Disconnected, "对端关闭应判为断链，实际: {e}");
    }

    #[test]
    fn 超时后残留输出不会污染下一条命令() {
        let f = spawn_fake(|mut s| {
            let first = read_line(&mut s);
            assert_eq!(first.split(" ; echo ").next().unwrap(), "slowcmd");
            // 迟到的输出：必须在客户端开始排空之后才到
            std::thread::sleep(Duration::from_millis(1500));
            s.write_all(b"STALE-FROM-CMD1\r\n").unwrap();

            // 客户端超时后会先发一条同步命令重新对齐会话
            let sync = read_line(&mut s);
            let sync_tok = sync.strip_prefix("echo ").unwrap().trim().to_string();
            assert!(sync_tok.starts_with('S'), "同步命令应以 S 开头: {sync}");
            s.write_all(format!("{}\r\nG7615> ", sync_tok).as_bytes()).unwrap();

            let second = read_line(&mut s);
            assert_eq!(second.split(" ; echo ").next().unwrap(), "fastcmd");
            let marker = marker_of(&second);
            s.write_all(b"FRESH\r\n").unwrap();
            s.write_all(format!("{}\r\nG7615> ", marker).as_bytes()).unwrap();
        });
        let mut t = test_link(&f.addr);
        let e = t.run_with_timeout("slowcmd", 1).unwrap_err();
        assert_eq!(e.kind, ErrKind::Timeout);
        let out = t.run_with_timeout("fastcmd", 5).unwrap();
        assert!(!out.contains("STALE"), "上一条的残留输出污染了下一条: {out:?}");
        assert_eq!(out, "FRESH");
    }

    /// B3 回归：超时后必须真正重新对齐，迟到输出不能被下一条当作自己的结果。
    /// 这一条让迟到输出在同步命令**之后**才到达，是 drain() 覆盖不到的时序。
    #[test]
    fn 同步命令之后才到的残留输出不会污染下一条命令() {
        let f = spawn_fake(|mut s| {
            let first = read_line(&mut s);
            assert_eq!(first.split(" ; echo ").next().unwrap(), "slowcmd");

            // 修复后的 resync 会「同步 → 观察静默 → 不安静就再同步」多轮。
            // 前两轮让陈旧输出迟到到同步标记之后，逼它继续同步；
            // 之后干净收尾，让它判定会话已对齐。
            // 注意：不假设命令到达顺序，按内容分派，避免时序竞态。
            let mut rounds = 0;
            loop {
                let line = read_line(&mut s);
                if let Some(sync_tok) = line.strip_prefix("echo ") {
                    let sync_tok = sync_tok.trim().to_string();
                    s.write_all(format!("{}\r\nG7615> ", sync_tok).as_bytes()).unwrap();
                    rounds += 1;
                    if rounds <= 2 {
                        // 陈旧输出迟到到同步标记之后——正是 drain() 窗口漏掉的情况
                        s.write_all(format!("STALE-ROUND{rounds}\r\n").as_bytes()).unwrap();
                    }
                    continue;
                }
                // 不是同步命令，那就是真正的下一条命令
                assert_eq!(line.split(" ; echo ").next().unwrap(), "fastcmd");
                let marker = marker_of(&line);
                s.write_all(b"FRESH\r\n").unwrap();
                s.write_all(format!("{}\r\nG7615> ", marker).as_bytes()).unwrap();
                break;
            }
            // 至少同步过一轮，否则这个测试就没验证到 resync
            assert!(rounds >= 1, "期望至少进行一轮同步，实际 {rounds} 轮");
        });
        let mut t = test_link(&f.addr);
        let e = t.run_with_timeout("slowcmd", 1).unwrap_err();
        assert_eq!(e.kind, ErrKind::Timeout);
        let out = t.run_with_timeout("fastcmd", 5).unwrap();
        assert!(!out.contains("STALE"), "同步后迟到的残留输出污染了下一条: {out:?}");
        assert_eq!(out, "FRESH");
    }

    #[test]
    fn 对端关闭按断链处理() {
        let f = spawn_fake(|mut s| {
            let _ = read_line(&mut s);
            drop(s);
        });
        let mut t = test_link(&f.addr);
        let e = t.run_with_timeout("reboot", 10).unwrap_err();
        assert_eq!(e.kind, ErrKind::Disconnected);
    }

    // ---------- connect() 登录链路（B10 补覆盖） ----------

    /// 把 `s` 里的字节一直读到换行，返回去掉 CR 的一行。
    fn read_line_from(s: &mut TcpStream) -> String {
        let mut out = Vec::new();
        let mut b = [0u8; 1];
        loop {
            match s.read(&mut b) {
                Ok(0) => break,
                Ok(_) => {
                    if b[0] == b'\n' {
                        break;
                    }
                    if b[0] != b'\r' {
                        out.push(b[0]);
                    }
                }
                Err(_) => break,
            }
        }
        String::from_utf8_lossy(&out).to_string()
    }

    /// 完整走一遍登录流程的假设备。
    /// `tag_mode` 决定最后一步 echo <tag> 怎么应答，
    /// 用来区分「登录真的成功」和「只回显了命令」。
    fn login_fake(tag_mode: TagMode) -> Fake {
        spawn_fake(move |mut s| {
            s.write_all(b"G7615 login: ").unwrap();
            read_line_from(&mut s); // 账号
            s.write_all(b"Password: ").unwrap();
            read_line_from(&mut s); // 密码
            s.write_all(b"\r\nG7615> ").unwrap();
            read_line_from(&mut s); // export PS1
            s.write_all(b"G7615> ").unwrap();
            let echo_line = read_line_from(&mut s); // echo <tag>
            let tag = echo_line.strip_prefix("echo ").unwrap_or("").trim().to_string();

            match tag_mode {
                // 只回显命令本身（含 tag 但整行不等）—— 密码错误时就是这种表现
                TagMode::EchoOnly => {
                    s.write_all(format!("{}\r\n", echo_line).as_bytes()).unwrap();
                }
                // tag 独占一行 —— 登录真的成功
                TagMode::WholeLine => {
                    s.write_all(format!("{}\r\nG7615> ", tag).as_bytes()).unwrap();
                }
            }
            // 保持连接不关，避免 connect 里读到 EOF；
            // 时间只要覆盖 connect 的判定即可，太长会拖慢测试
            std::thread::sleep(Duration::from_secs(3));
        })
    }

    #[derive(Clone, Copy)]
    enum TagMode {
        EchoOnly,
        WholeLine,
    }

    #[test]
    fn 登录tag整行匹配才算成功() {
        let f = login_fake(TagMode::WholeLine);
        let host_port: Vec<&str> = f.addr.split(':').collect();
        assert!(
            Telnet::connect(host_port[0], host_port[1], "root", "Zte521").is_ok(),
            "tag 整行出现时应判定登录成功"
        );
    }

    #[test]
    fn 登录只回显命令时不能误判成功() {
        let f = login_fake(TagMode::EchoOnly);
        let host_port: Vec<&str> = f.addr.split(':').collect();
        let err = Telnet::connect(host_port[0], host_port[1], "root", "Zte521").unwrap_err();
        assert_eq!(err.kind, ErrKind::Disconnected);
        // 错误信息应指向「登录未成功」，而不是别的
        let msg = err.to_string();
        assert!(
            msg.contains("登录") || msg.contains("密码") || msg.contains("拒绝"),
            "错误文案应提示登录相关，实际: {msg}"
        );
    }

    /// B7 回归：主机名必须能解析，不能因为 `parse::<SocketAddr>()` 而必然失败。
    #[test]
    fn 主机名可以解析() {
        // 对端真的在 listen 且什么都不发；connect 会在等登录提示符时超时。
        // 关键点是：错误必须是「没收到登录提示」，而不是「地址无法解析」。
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port().to_string();
        // accept 出来的连接要一直持有，否则对端会立刻收到 RST
        let h = std::thread::spawn(move || {
            let mut conns = Vec::new();
            conns.push(l.accept());
            std::thread::sleep(Duration::from_secs(15));
            drop(conns);
        });
        // LOGIN_TIMEOUT_SECS = 10，这里给足余量
        let err = Telnet::connect("localhost", &port, "root", "Zte521").unwrap_err();
        let msg = err.to_string();
        // 后台线程会自己退出，不 join 以免测试挂住
        drop(h);
        assert!(
            !msg.contains("地址无法解析"),
            "主机名应能解析，不应报地址无法解析: {msg}"
        );
        assert!(
            msg.contains("登录提示"),
            "主机名解析成功时应走到等待登录提示这一步，实际: {msg}"
        );
    }

    #[test]
    fn 端口非数字时给出明确错误() {
        let err = Telnet::connect("127.0.0.1", "notaport", "root", "Zte521").unwrap_err();
        assert_eq!(err.kind, ErrKind::Disconnected);
        assert!(
            err.to_string().contains("端口") || err.to_string().contains("地址"),
            "应给出端口/地址相关的明确错误，实际: {}",
            err
        );
    }
}