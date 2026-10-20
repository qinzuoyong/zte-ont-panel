//! 界面层：顶栏 / 侧栏 / 中央日志 / 底部命令条 / 回读校验卡片。
//!
//! 这个模块只负责「把状态画出来、把点击翻译成消息」。
//! 颜色、字号、间距、尺寸一律来自 `theme` ——
//! 本文件里除 `0.0`（表示贴边）之外不出现任何裸数字。
//!
//! 消息通路与改造前**一一对应**：连接/断开、参数表单、危险确认、
//! 自定义命令、表结构查询、机型识别、回读校验、日志落盘，一个都没少。

use eframe::egui;
use std::time::Duration;

use crate::theme::{self, Palette, Tone};
use crate::{
    expand_placeholders, App, CriticalForm, ParamForm, ToWorker, ToUi,
    VerifyResult,
};

// ---------------- 界面专用的纯视觉状态 ----------------

/// 日志里一行的语义分类。**只影响颜色，不影响内容**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogKind {
    /// 普通输出
    Plain,
    /// 分节标题
    Heading,
    /// 命令回显
    Command,
    /// 分隔线
    Rule,
    /// 已确认 / 已连接 / 已就绪
    Success,
    /// 错误 / 失败 / 中断
    Error,
    /// 无法确认 / 超时 / 提示
    Warning,
    /// 系统提示
    Notice,
}

impl LogKind {
    /// 按行首特征分类。
    ///
    /// 顺序有讲究：先判最具体的标记，再判宽松的。
    /// 否则「回读校验未通过」那条会被当成普通分隔线染成灰色，
    /// 把「有写命令没通过」这条关键信息淹掉。
    pub fn of(line: &str) -> Self {
        let t = line.trim();
        if t.is_empty() {
            return LogKind::Plain;
        }
        if t.starts_with("=====") {
            return LogKind::Heading;
        }
        if t.starts_with('>') {
            return LogKind::Command;
        }
        if t.contains("未通过回读校验") || t.contains("可能没有真正写入") {
            return LogKind::Error;
        }
        if t.contains("已确认生效") || t.starts_with("[已就绪]") || t.starts_with("[已连接") {
            return LogKind::Success;
        }
        if t.contains("无法确认") || t.contains("[超时]") || t.starts_with("注意：") {
            return LogKind::Warning;
        }
        if t.starts_with("-----") {
            return LogKind::Rule;
        }
        if t.starts_with("[错误]")
            || t.starts_with("[连接失败]")
            || t.starts_with("[预设文件解析失败]")
        {
            return LogKind::Error;
        }
        if t.starts_with('[') {
            return LogKind::Notice;
        }
        LogKind::Plain
    }

    pub fn color(self, pal: &Palette) -> egui::Color32 {
        match self {
            LogKind::Plain => pal.text_secondary,
            LogKind::Heading => pal.text_primary,
            LogKind::Command => pal.accent,
            LogKind::Rule => pal.text_muted,
            LogKind::Success => pal.success,
            LogKind::Error => pal.danger,
            LogKind::Warning => pal.warning,
            LogKind::Notice => pal.info,
        }
    }
}

// ---------------- 点击路由 ----------------

/// 点一个操作按钮之后会发生什么。
///
/// 抽成纯函数是为了让「危险操作绝不能直接执行」变成**可断言**的规则：
/// 只要 `route` 对 danger 永远不返回 `Direct`，二次确认就不可能被绕过。
/// 这段逻辑散在界面闭包里时，只能靠人眼复查。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// 先弹参数表单（表单里提交后，危险操作还要再走一次确认）
    ParamForm,
    /// 直接弹危险确认
    DangerConfirm,
    /// 刷机级操作：走专用闸门（备份 → 三勾选 → 双输入 → 执行），
    /// **不经过**普通危险确认，也绝不直接下发。
    Critical,
    /// 无需确认，直接下发
    Direct,
}

/// 点击路由。
///
/// 判定顺序有讲究：`critical` 必须排在 `needs_input` **之前**，
/// 否则带参数的刷机级操作会先落进普通参数表单，
/// 用户填完点「执行」就绕过了备份与三段确认。
pub fn route(action: &zte_g7615_panel::presets::Action) -> Route {
    if action.critical {
        Route::Critical
    } else if action.needs_input() {
        Route::ParamForm
    } else if action.danger {
        Route::DangerConfirm
    } else {
        Route::Direct
    }
}

// ---------------- 可操作性判定 ----------------

/// 「现在能不能把命令发出去」的纯函数版本。
///
/// 抽出来是为了能被单测直接驱动：`App` 的构造会加载预设、建日志文件、
/// 甚至搬目录，测试里不能随便造一个出来。
///
/// 四个条件缺一不可，其中 `modal_open` 是这一轮补上的：
/// 模态只挡鼠标，不挡键盘快捷键，缺了它闸门开着也能发命令。
pub fn can_send_gate(connected: bool, busy: bool, worker_dead: bool, modal_open: bool) -> bool {
    connected && !busy && !worker_dead && !modal_open
}

/// 一条 `CriticalBackupDone` 是不是过期结论。
///
/// `form_token` 是**当前打开的**闸门记下的序号；闸门已经关掉时传 `None`。
/// 序号对不上（或闸门已关）就说明这条结论来自上一轮 —— 必须丢弃，
/// 否则「取消后重开」的闸门会立刻显示已备份，而这次一次备份都没跑。
pub fn backup_result_is_stale(form_token: Option<u64>, incoming: u64) -> bool {
    match form_token {
        Some(t) => t != incoming,
        None => true,
    }
}

// ---------------- 日志行高 ----------------

/// 一条日志行在当前宽度下会不会折成多行。
///
/// `per_line` 是「一行大概能放下几个半角字符」。中日韩字符按两格算
/// （等宽字体里它们通常是两个半角宽），这样估算才不会把中文行判成不折行。
///
/// 这个判断只用于选渲染路径：判宽了最多多花一点排版时间，
/// 判窄了会让滚到底时末尾被切掉 —— 所以宁可判宽。
pub fn line_wraps(line: &str, per_line: usize) -> bool {
    if per_line == usize::MAX {
        return false;
    }
    let mut width = 0usize;
    for ch in line.chars() {
        width += if ch.is_ascii() { 1 } else { 2 };
        if width > per_line {
            return true;
        }
    }
    false
}

// ---------------- 输入校验 ----------------

/// 表名是否允许拼进 `sendcmd 1 DB p <表名>`。
///
/// ## 为什么必须校验
///
/// 「表结构查询」的表名是用户直接输入的，而它会被拼成一条命令下发。
/// `Telnet::send_line` 只是把字符串加 `\r\n` 写出去，**不会**过滤换行 ——
/// 所以表名里塞一个 `\r\nreboot` 就等于让设备多执行一条 `reboot`：
/// 界面上只是「读了一张表」，设备却重启了。
///
/// 同一条通道上还有 `xx ; echo <marker>` 的结束标记拼接，
/// 输入里的 `;` 也能插入额外命令。
///
/// 光猫的表名实际形态是 `DevInfo`、`WLANCfg`、`PONINFO` 这类
/// 纯 ASCII 字母数字，收紧成这个形状不影响任何正常用法。
pub fn is_safe_table_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---------------- 主体 ----------------

impl App {
    /// 改区域前必须备份的表。
    ///
    /// 选表依据：`upgradetest sdefconf` 会把配置重置成省默认模板，
    /// 受影响最重的是「账号口令 / WAN / WiFi / Telnet / 管理服务器 /
    /// 注册信息」这几类。逐张 `p` 出来存盘，事后至少能查出原来是什么。
    ///
    /// 不追求「全表备份」：`sendcmd 1 DB p` 一次只能读一张表，
    /// 表名也因固件而异，盲列未验证的表只会刷出一屏报错。
    pub(crate) const CRITICAL_BACKUP_TABLES: &'static [&'static str; 8] = &[
        "DevAuthInfo",
        "DevInfo",
        "TelnetCfg",
        "WANC",
        "WLANCfg",
        "MgtServer",
        "UserInfo",
        "PONINFO",
    ];

    /// 接收 worker 消息。返回本帧是否消费到了消息（用于请求重绘）。
    pub fn drain_messages(&mut self) -> bool {
        let mut got_msg = false;
        while let Ok(m) = self.rx.try_recv() {
            got_msg = true;
            match m {
                ToUi::Log(s) => self.append_log(&s),
                ToUi::Status(c) => {
                    // 连接成功那一刻把地址/端口/账号记下来。
                    // 密码不落盘 —— 详见 settings.rs 顶部的安全说明。
                    if c && !self.connected {
                        let s = zte_g7615_panel::settings::ConnectionSettings {
                            host: self.host.trim().to_string(),
                            port: self.port.trim().to_string(),
                            user: self.user.trim().to_string(),
                        };
                        if let Some(p) = zte_g7615_panel::settings::settings_path() {
                            // 存不下不算致命：下次多填一遍而已，不该打断用户
                            let _ = zte_g7615_panel::settings::save_to(&p, &s);
                        }
                    }
                    self.connected = c;
                }
                ToUi::Connecting(c) => self.connecting = c,
                ToUi::Busy(b) => self.busy = b,
                ToUi::VerifyCleared => self.verifications.clear(),
                ToUi::Verify { cmd, result, detail } => {
                    self.verifications.insert(0, (cmd, result, detail));
                    // 只保留最近若干条（上限见 theme::VERIFY_HISTORY_MAX），避免无限增长
                    self.verifications.truncate(theme::VERIFY_HISTORY_MAX);
                }
                ToUi::ModelDetected { model, removed, source, groups } => {
                    // 换整套按钮 + 更新来源显示
                    self.detected_model = Some(model);
                    self.hidden_commands = removed;
                    self.preset_source = source;
                    self.preset_error = None;
                    // 关键：真正把按钮换掉。少了这一步，机型识别就只影响日志。
                    if !groups.is_empty() {
                        self.groups = groups;
                    }
                }
                ToUi::WorkerDead(why) => {
                    self.worker_dead = true;
                    self.worker_dead_reason = why;
                    // 闸门的 busy 也必须一起清掉。
                    //
                    // 这是我引入 CriticalBackup 时差点留下的回归：
                    // 若 worker 在备份途中 panic，`CriticalBackupDone` 永远
                    // 不会到达，`form.busy` 就永久停在 true ——
                    // 「备份并检查」按钮从此灰着，而界面又不说为什么。
                    if let Some(f) = self.critical.as_mut() {
                        f.busy = false;
                        if f.backup_path.is_none() && f.backup_error.is_none() {
                            f.backup_error =
                                Some("后台线程已停止，备份没有完成。请重启程序。".into());
                        }
                    }
                }
                ToUi::CriticalBackupDone { token, path, error, region_detected, tables } => {
                    // 过期的结论一律丢弃。
                    //
                    // 闸门可以「取消」后再打开，而 worker 那条备份还在跑；
                    // 不比对序号的话，上一轮的结论会落到新表单上，
                    // 把一次**从没跑过备份**的打开直接解锁到第 ②③ 步。
                    // 另外，闸门已经关掉（critical 为 None）时也不能留下任何痕迹。
                    let stale = backup_result_is_stale(
                        self.critical.as_ref().map(|f| f.backup_token),
                        token,
                    );
                    if stale {
                        // 用 continue 而不是 return：后面可能还排着别的消息
                        // （日志、Busy(false) 等），提前返回会把它们一起漏掉。
                        continue;
                    }
                    if let Some(f) = self.critical.as_mut() {
                        // 只有真的拿到路径才置成功：None 会让闸门保持锁定。
                        f.backup_path = path;
                        f.backup_error = error;
                        f.backup_tables = tables.clone();
                        f.busy = false;
                    }
                    // 关键表没备全就放行，等于给用户一份「看着像备份」的摆设。
                    // 不阻断（有几张读不到是设备固件差异），但必须说出来是哪几张。
                    if !tables.is_empty() {
                        let missing: Vec<&str> = Self::CRITICAL_BACKUP_TABLES
                            .iter()
                            .copied()
                            .filter(|t| !tables.iter().any(|x| x == t))
                            .collect();
                        if !missing.is_empty() {
                            self.append_log(&format!(
                                "[区域闸门] 关键表只备到 {}/{} 张，缺：{}。\n",
                                tables.len(),
                                Self::CRITICAL_BACKUP_TABLES.len(),
                                missing.join("、")
                            ));
                        }
                    }
                    // 备份成功但没读到区域码时，必须单独提示：
                    // 用户接下来要手填「当前区域码」，填错就白备份。
                    if !region_detected {
                        self.append_log(
                            "[区域闸门] 未能从输出读到当前区域码，请人工核对后再填写。\n",
                        );
                    }
                }
            }
        }
        got_msg
    }

    /// 一帧的全部绘制。
    pub fn draw(&mut self, ui: &mut egui::Ui) {
        let got_msg = self.drain_messages();

        // 当前主题的配色，整帧共用一份。
        // 每个绘制函数都把它传下去，而不是各自去读全局状态 ——
        // 这样「这一帧用的是哪套颜色」在签名上就看得见。
        let pal = *theme::palette(ui.visuals().dark_mode);

        // egui 默认事件驱动重绘：mpsc 里有消息不算输入事件，
        // 不主动请求重绘的话状态灯和日志要等用户动一下鼠标才更新。
        if got_msg {
            ui.ctx().request_repaint();
        }
        if self.busy || self.connecting {
            ui.ctx()
            .request_repaint_after(Duration::from_millis(theme::REPAINT_INTERVAL_MS));
        }

        self.handle_shortcuts(ui.ctx());

        // 面板顺序决定占位顺序：先顶栏与底部，再左右，中央吃剩下的
        self.top_bar(ui, &pal);
        self.command_bar(ui, &pal);
        self.verification_card(ui, &pal);
        self.side_panel(ui, &pal);
        self.log_panel(ui, &pal);

        // 阻断性浮层最后画，保证盖在所有面板之上
        self.param_modal(ui.ctx(), &pal);
        self.danger_modal(ui.ctx(), &pal);
        self.critical_modal(ui.ctx(), &pal);
    }

    // ---------- 快捷键 ----------

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let (ctrl_k, ctrl_enter) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::CTRL, egui::Key::K),
                i.consume_key(egui::Modifiers::CTRL, egui::Key::Enter),
            )
        });

        if ctrl_k {
            // 把焦点塞进搜索框：用户按 Ctrl+K 之后要能直接打字。
            // 搜索框收起时先展开，否则焦点会落到看不见的控件上。
            if !self.show_search {
                self.show_search = true;
            }
            ctx.memory_mut(|m| m.request_focus(egui::Id::new("sidebar_search")));
        }

        if ctrl_enter && self.can_send() {
            let c = self.custom.trim().to_string();
            if !c.is_empty() {
                let _ = self.tx.send(ToWorker::RunRaw { cmd: c });
                self.custom.clear();
            }
        }
    }

    // ---------- 可操作性判定 ----------

    /// 是否有阻断性模态开着（危险确认 / 参数表单 / 刷机级闸门）。
    ///
    /// 模态只挡住了**鼠标**：遮罩让底下的控件点不到，
    /// 但 `handle_shortcuts` 读的是原始键盘事件，命令条的输入框也还能编辑。
    /// 于是闸门开着、三项一条没勾的时候按 Ctrl+Enter，
    /// 一条命令就绕过备份与三段确认发了出去 —— 而用户以为自己在闸门里。
    fn modal_open(&self) -> bool {
        self.pending.is_some() || self.param_form.is_some() || self.critical.is_some()
    }

    fn can_send(&self) -> bool {
        can_send_gate(self.connected, self.busy, self.worker_dead, self.modal_open())
    }

    /// 侧栏操作按钮是否可点。
    ///
    /// 有待确认的危险操作 / 参数表单时禁止再点：
    /// 否则新操作会覆盖掉正在填的表单，用户填的内容白填。
    fn actions_enabled(&self) -> bool {
        self.can_send()
    }

    // ---------- 顶栏 ----------

    /// 顶栏：两段式。
    ///
    /// 第 1 行是「现在是什么状态」，第 2 行是「要连到哪里去」。
    /// 合成一行时四个输入框只能靠 placeholder 自报家门，
    /// 填上值之后就再也看不出哪格是端口、哪格是账号了。
    ///
    /// 标签放在输入框**上方**而不是左侧：在上只吃纵向空间，
    /// 窄窗口不会因为标签而把输入框挤出屏幕。
    fn top_bar(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        egui::Panel::top("conn")
            .frame(theme::topbar_frame(pal))
            .show_separator_line(false)
            .show(ui, |ui| {
                // 顶栏高度**锁死**：min 与 max 都是 TOPBAR_HEIGHT。
                // 只设 min 的话，任何一次子布局失误都会把面板撑高，
                // 而撑高的表现是「输入框跑到屏幕外」—— 用户看到的就是启动失败。
                ui.set_height(theme::TOPBAR_HEIGHT);
                ui.set_min_height(theme::TOPBAR_HEIGHT);
                ui.set_max_height(theme::TOPBAR_HEIGHT);

                // ---- 第 1 行：品牌 / 机型 / 状态 / 外观 ----
                // 用 horizontal 而不是 horizontal_centered：后者在面板里
                // 会按可用高度居中，两行布局下会把第一行顶到面板正中。
                ui.horizontal(|ui| {
                    ui.label(theme::body("中兴光猫", pal).strong());
                    ui.add_space(theme::SPACE_8);
                    match self.detected_model {
                        Some(m) => {
                            theme::badge(ui, pal, m.display_name(), Tone::Info);
                        }
                        None => {
                            theme::badge(ui, pal, "机型待识别", Tone::Neutral);
                        }
                    }

                    ui.add_space(theme::SPACE_21);
                    let (color, label) = if self.worker_dead {
                        (pal.warning, "已停用")
                    } else if self.connecting {
                        (pal.accent, "连接中…")
                    } else if self.connected {
                        (pal.success, "已连接")
                    } else {
                        (pal.danger, "未连接")
                    };
                    theme::status_dot(ui, color);
                    // worker 已死时把原因挂在悬停提示上：
                    // 状态字只能写「已停用」，可用户需要知道的是「为什么」。
                    let status = ui.label(theme::caption(label, pal));
                    if self.worker_dead && !self.worker_dead_reason.is_empty() {
                        status.on_hover_text(format!(
                            "工作线程已退出，命令功能停用，请重启程序。\n原因：{}",
                            self.worker_dead_reason
                        ));
                    }
                    // 连接胶囊只是「一眼确认连的是哪台」，不是必需信息：
                    // 窗口窄到放不下时直接不画，而不是让它压到右侧控件上。
                    if !self.host.trim().is_empty()
                        && ui.available_width() >= theme::CAPSULE_MIN_W
                    {
                        ui.label(theme::caption(
                            format!(
                                "{}:{} · {}",
                                self.host.trim(),
                                self.port.trim(),
                                self.user.trim()
                            ),
                            pal,
                        ));
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        self.appearance_menu(ui, pal);
                    });
                });

                ui.add_space(theme::SPACE_8);

                // ---- 第 2 行：四个字段 + 连接按钮 ----
                //
                // ⚠ 按钮列**必须**用「vertical + 顶部留白」来和输入框底部对齐，
                // 不能用 `with_layout(right_to_left(Align::BOTTOM))`：
                // 在 Panel::top 里，那个布局会吃掉整个剩余高度，
                // 把顶栏撑成一屏高、把四个输入框挤到可视区外（v1.1 首版真实事故）。
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = theme::SPACE_13;

                    theme::labeled_input(ui, pal, "光猫 IP", &mut self.host, theme::FIELD_W);
                    theme::labeled_input(ui, pal, "端口", &mut self.port, theme::FIELD_W_PORT);
                    theme::labeled_input(ui, pal, "Telnet 账号", &mut self.user, theme::FIELD_W);
                    let (_, picked) = theme::password_input(
                        ui,
                        pal,
                        "Telnet 密码",
                        &mut self.pwd,
                        &mut self.show_pwd,
                        theme::FIELD_W_PWD,
                        Some(zte_g7615_panel::passwords::PRESETS),
                    );
                    // 选中常用口令后：口令已由 password_input 写入，
                    // 这里只决定账号要不要跟着填 —— 用户自己敲的账号优先。
                    if let Some(p) = picked {
                        let (u, _) = zte_g7615_panel::passwords::apply(
                            p,
                            &self.user,
                            &self.pwd,
                        );
                        self.user = u;
                    }

                    // 按钮贴右：**显式**留出剩余宽度，而不是靠 with_layout 的
                    // 宽度语义 —— 后者在 Panel 里行为随版本变，实测不贴边。
                    let button_w = theme::SPACE_89;
                    let slack = ui.available_width() - button_w;
                    if slack > 0.0 {
                        ui.add_space(slack);
                    }
                    // 先空出标签那一行的高度，按钮就落在输入框这一行上
                    ui.vertical(|ui| {
                        ui.add_space(theme::LABEL_H + theme::SPACE_2);
                        if self.connected {
                            if ui.add(theme::ghost_button("断开", pal)).clicked() {
                                let _ = self.tx.send(ToWorker::Disconnect);
                            }
                        } else {
                            let can = !self.connecting && !self.busy && !self.worker_dead;
                            let text = if self.connecting { "连接中…" } else { "连接" };
                            if ui
                                .add_enabled(can, theme::primary_button(text, pal))
                                .clicked()
                            {
                                let _ = self.tx.send(ToWorker::Connect {
                                    host: self.host.clone(),
                                    port: self.port.clone(),
                                    user: self.user.clone(),
                                    pwd: self.pwd.clone(),
                                });
                            }
                        }
                    });
                });
            });
    }

    /// 外观菜单：浅色 / 深色 / 跟随系统，三态单选。
    ///
    /// 不做「点一下就切」的两态按钮：两态没法表达「跟随系统」，
    /// 而且当前是跟随系统时，按钮该显示太阳还是月亮没有答案。
    fn appearance_menu(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let current = self.theme_pref;
        let label = match current {
            egui::ThemePreference::Light => "外观 · 浅色",
            egui::ThemePreference::Dark => "外观 · 深色",
            egui::ThemePreference::System => "外观 · 跟随系统",
        };
        // menu_button 收的是 WidgetText 而不是 Button，
        // 所以这里给一个与 ghost_button 同色的纯文字，视觉上仍像按钮。
        let text = egui::RichText::new(label)
            .size(theme::TEXT_BODY)
            .color(pal.text_secondary);
        ui.menu_button(text, |ui| {
            let mut chosen = current;
            ui.selectable_value(&mut chosen, egui::ThemePreference::Light, "浅色");
            ui.selectable_value(&mut chosen, egui::ThemePreference::Dark, "深色");
            ui.selectable_value(&mut chosen, egui::ThemePreference::System, "跟随系统");
            ui.add_space(theme::SPACE_4);
            ui.label(theme::caption("切换立即生效，下次启动沿用", pal));
            if chosen != current {
                self.set_theme_pref(ui.ctx(), chosen);
                ui.close();
            }
        });
    }

    // ---------- 侧栏 ----------

    fn side_panel(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        let icon_mode = self.sidebar_width < theme::SIDEBAR_ICON_BREAKPOINT;
        egui::Panel::left("actions")
            .resizable(true)
            .default_size(self.sidebar_width)
            .min_size(theme::SIDEBAR_MIN)
            .max_size(theme::SIDEBAR_MAX)
            .frame(theme::sidebar_frame(pal))
            .show_separator_line(false)
            .show(ui, |ui| {
                // 侧栏宽度由用户拖拽决定；下一帧用它判断要不要进图标模式
                self.sidebar_width = ui.available_width();
                egui::ScrollArea::vertical()
                    .id_salt("actions_scroll")
                    .auto_shrink([false, false])
                    .show(ui, |ui| self.sidebar_content(ui, pal, icon_mode));
            });
    }

    fn sidebar_content(&mut self, ui: &mut egui::Ui, pal: &Palette, icon_mode: bool) {
        // ---- 搜索 ----
        if self.show_search {
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.search)
                    .id(egui::Id::new("sidebar_search"))
                    .hint_text("搜索操作  Ctrl+K")
                    .desired_width(f32::INFINITY),
            );
            if resp.changed() {
                ui.ctx().request_repaint();
            }
            ui.add_space(theme::SPACE_8);
        }

        // ---- 预设来源 + 机型：压成一行弱文字，详情走 tooltip ----
        let src_line = if self.preset_error.is_some() {
            "⚠ 预设解析失败，已用内置清单"
        } else {
            "预设来源"
        };
        let mut tip = format!("来源：{}", self.preset_source);
        if let Some(e) = &self.preset_error {
            tip.push_str(&format!("\n解析错误：{e}"));
        }
        if let Some(m) = self.detected_model {
            tip.push_str(&format!("\n机型：{}", m.display_name()));
        }
        if !self.hidden_commands.is_empty() {
            tip.push_str(&format!(
                "\n已隐藏 {} 条本机型不支持的命令",
                self.hidden_commands.len()
            ));
        }
        ui.label(theme::caption(src_line, pal)).on_hover_text(tip);
        ui.add_space(theme::SPACE_13);

        // ---- 分组与操作 ----
        let query = self.search.trim().to_lowercase();
        let groups = self.groups.clone();
        let actions_enabled = self.actions_enabled();
        let mut open_form: Option<ParamForm> = None;
        let mut danger_pending: Option<(String, Vec<String>, Option<String>)> = None;
        let mut critical_pending: Option<(String, Vec<String>, Option<String>)> = None;
        let mut run_now: Option<(String, Vec<String>)> = None;

        for (gi, (group, actions)) in groups.iter().enumerate() {
            let hits: Vec<_> = actions
                .iter()
                .filter(|a| {
                    query.is_empty()
                        || a.name.to_lowercase().contains(&query)
                        || group.to_lowercase().contains(&query)
                })
                .cloned()
                .collect();
            if hits.is_empty() {
                continue;
            }

            let id = ui.make_persistent_id(("group", gi));
            // 有搜索词时全展开（让命中项直接可见）；
            // --open-groups 时也全展开，供验收截图看清操作行与危险标记。
            let default_open = !query.is_empty() || self.open_all_groups;
            let title = if icon_mode {
                group.chars().take(1).collect::<String>()
            } else {
                group.clone()
            };
            // 自绘标题：三角 + 名称 + 计数，悬停高亮，点哪都算点标题
            let (header, open) =
                theme::collapsible_header(ui, pal, id, &title, Some(hits.len()), default_open);
            header.on_hover_text(group);

            if !open {
                continue;
            }

            ui.add_space(theme::SPACE_4);
            ui.indent(("g", gi), |ui| {
                for a in &hits {
                    let label = if icon_mode {
                        a.name.chars().take(1).collect::<String>()
                    } else {
                        a.name.clone()
                    };
                    let resp = theme::action_row(ui, pal, &label, a.danger, actions_enabled)
                        .on_hover_text(format!(
                            "{}{}",
                            a.name,
                            a.note
                                .as_deref()
                                .map(|n| format!("\n{n}"))
                                .unwrap_or_default()
                        ));

                    // 键盘激活：Tab 到这一行之后按 Enter / 空格等同于点击。
                    // 自绘行没有 egui 内置按钮的那套键盘处理，必须自己接，
                    // 否则「Tab 顺序合理」只是焦点能走、却什么也做不了。
                    let keyboard_activate =
                        resp.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Space));
                    if (resp.clicked() || keyboard_activate) && actions_enabled {
                        match route(a) {
                            Route::ParamForm => {
                                open_form = Some(ParamForm {
                                    title: a.name.clone(),
                                    cmds: a.cmds.clone(),
                                    note: a.note.clone(),
                                    fields: a
                                        .params
                                        .iter()
                                        .map(|p| (p.clone(), String::new()))
                                        .collect(),
                                    error: None,
                                    danger: a.danger,
                                });
                            }
                            Route::DangerConfirm => {
                                danger_pending =
                                    Some((a.name.clone(), a.cmds.clone(), a.note.clone()));
                            }
                            Route::Critical => {
                                // 刷机级：交给专用闸门，**不**塞进 pending，
                                // 否则会走普通危险确认，备份与三段勾选就全绕过了。
                                critical_pending =
                                    Some((a.name.clone(), a.cmds.clone(), a.note.clone()));
                            }
                            Route::Direct => {
                                run_now = Some((a.name.clone(), expand_placeholders(&a.cmds)));
                            }
                        }
                    }
                }
            });
            ui.add_space(theme::SPACE_13);
        }

        // ---- 表结构查询（收进折叠区）----
        ui.add_space(theme::SPACE_8);
        theme::hairline(ui, pal);
        ui.add_space(theme::SPACE_8);
        let enabled = self.can_send();
        let (_, schema_open) = theme::collapsible_header(
            ui,
            pal,
            ui.make_persistent_id("schema_tools"),
            "表结构查询",
            None,
            false,
        );
        if schema_open {
            ui.indent("schema_body", |ui| {
                if ui
                    .add_enabled(enabled, theme::ghost_button("列出全部表名", pal))
                    .clicked()
                {
                    let _ = self.tx.send(ToWorker::RunRaw {
                        cmd: "sendcmd 1 DB all".into(),
                    });
                }
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.table_query)
                            .hint_text("表名")
                            .desired_width(theme::SPACE_89),
                    );
                    // 表名会被拼进命令下发，必须只放行「纯字母数字下划线」；
                    // 否则 `DevInfo\r\nreboot` 会让设备多执行一条命令。
                    let table_ok = is_safe_table_name(self.table_query.trim());
                    let can_q = enabled && table_ok;
                    if ui
                        .add_enabled(can_q, theme::ghost_button("读整表", pal))
                        .clicked()
                    {
                        let _ = self.tx.send(ToWorker::RunRaw {
                            cmd: format!("sendcmd 1 DB p {}", self.table_query.trim()),
                        });
                    }
                    if !self.table_query.trim().is_empty() && !table_ok {
                        ui.label(theme::caption(
                            "表名只能包含字母、数字、下划线",
                            pal,
                        ));
                    }
                });
            });
        }

        // 闭包外统一应用（避免边遍历边改 self）
        if let Some(f) = open_form {
            self.param_form = Some(f);
        }
        if let Some(d) = danger_pending {
            self.pending = Some(d);
        }
        if let Some(c) = critical_pending {
            self.begin_critical(c);
        }
        if let Some((title, cmds)) = run_now {
            let _ = self.tx.send(ToWorker::Run { title, cmds });
        }
    }

    // ---------- 中央日志 ----------

    fn log_panel(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::NONE.fill(pal.bg_panel).inner_margin(egui::Margin::symmetric(
                    theme::SPACE_13 as i8,
                    theme::SPACE_8 as i8,
                )),
            )
            .show(ui, |ui| {
                let line_count = self.log.lines().count();
                ui.horizontal(|ui| {
                    ui.label(theme::body("输出", pal).strong());
                    ui.label(theme::caption(format!("{line_count} 行"), pal));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(theme::ghost_button("复制", pal)).clicked() {
                            ui.ctx().copy_text(self.log.clone());
                        }
                        if ui.add(theme::ghost_button("清空", pal)).clicked() {
                            self.log.clear();
                        }
                        ui.checkbox(&mut self.auto_scroll, "自动滚动");
                    });
                });
                ui.add_space(theme::SPACE_8);
                theme::hairline(ui, pal);
                ui.add_space(theme::SPACE_8);

                // 空状态：不留一片空白，明确告诉用户下一步该做什么
                if self.log.trim().is_empty() {
                    ui.vertical_centered(|ui| {
                        ui.add_space(theme::SPACE_89);
                        ui.label(theme::display("还没有输出", pal));
                        ui.add_space(theme::SPACE_13);
                        ui.label(theme::secondary(
                            if self.connected {
                                "在左侧选一个操作，或直接在底部输入命令"
                            } else {
                                "先在顶栏填好光猫 IP、账号与密码，点「连接」"
                            },
                            pal,
                        ));
                    });
                    return;
                }

                let log = self.log.clone();
                let lines: Vec<&str> = log.lines().collect();
                let total = lines.len();
                // 只排可视区附近的行：日志上限 256KB，
                // 整段逐行排版会明显掉帧。
                //
                // ⚠ 行高必须和日志区实际样式一致。
                //
                // `show_rows` 只画 [首行, 末行] 这一段，并且**整段放在第 min_row
                // 行的位置上**。以前这里传的是 `theme::LINE_HEIGHT`（21），
                // 而 `.wrap()` 的 Label 单行实际只占约 16.4px —— 加上 item_spacing
                // 是 24.4pt。于是滚到底时首行被算成第 ~1300 行，
                // 那一段被摆到 1300×24.4 ≈ 31700pt 处，而内容总高只有 30300pt：
                // **日志末尾（`----- 完成 -----` 这类结论）被画到了可视区外**，
                // 屏幕上显示的是更早、已经滚过去的行。
                // 长日志一多，用户看到的「完成」就不是这一批的结论。
                let row_h = ui.text_style_height(&egui::TextStyle::Monospace);
                // `show_rows` 的前提是**每行等高**，而日志开了 `.wrap()`：
                // 一条超长行（某个字段的 val 特别长）会占两三行，
                // 总高度被低估 → 滚到底时最后几行被画到可视区外，
                // 屏幕上停住的不是最新的输出。
                //
                // 所以先估一下「有没有行会折行」：有就退回整段渲染
                // （排版精确，代价是全量布局，但日志本身封顶 256KB）；
                // 没有才走虚拟化快路径。
                let char_w = ui.fonts_mut(|f| {
                    f.glyph_width(&egui::FontId::monospace(theme::TEXT_BODY), ' ')
                });
                let per_line = if char_w > 0.0 {
                    (ui.available_width() / char_w).floor().max(1.0) as usize
                } else {
                    usize::MAX
                };
                let any_wrapped = lines.iter().any(|l| line_wraps(l, per_line));

                let scroll = egui::ScrollArea::vertical()
                    .id_salt("log_scroll")
                    .auto_shrink([false, false])
                    .stick_to_bottom(self.auto_scroll);
                let paint = |ui: &mut egui::Ui, range: std::ops::Range<usize>| {
                    for line in &lines[range] {
                        let kind = LogKind::of(line);
                        ui.add(
                            egui::Label::new(theme::mono(*line, kind.color(pal)))
                                .wrap()
                                .selectable(true),
                        );
                    }
                };
                if any_wrapped {
                    // 有折行 → 行高不再一致，虚拟化会把末尾算错位。
                    scroll.show(ui, |ui| paint(ui, 0..total));
                } else {
                    scroll.show_rows(ui, row_h, total, |ui, range| paint(ui, range));
                }
            });
    }

    // ---------- 回读校验卡片 ----------

    fn verification_card(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        if self.verifications.is_empty() {
            return;
        }
        let confirmed = self
            .verifications
            .iter()
            .filter(|(_, r, _)| *r == VerifyResult::Confirmed)
            .count();
        let failed = self
            .verifications
            .iter()
            .filter(|(_, r, _)| *r == VerifyResult::Failed)
            .count();
        let masked = self.verifications.len() - confirmed - failed;

        egui::Panel::bottom("verify")
            .resizable(true)
            .default_size(theme::VERIFY_PANEL_H)
            .frame(
                egui::Frame::NONE.fill(pal.bg_panel).inner_margin(egui::Margin::symmetric(
                    theme::SPACE_13 as i8,
                    theme::SPACE_8 as i8,
                )),
            )
            .show_separator_line(false)
            .show(ui, |ui| {
                theme::card_frame(pal).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(theme::body("回读校验", pal).strong());
                        // 三态计数：颜色 + 形状（图标）+ 数字，三重区分
                        theme::badge(ui, pal, &format!("✅ {confirmed}"), Tone::Success);
                        theme::badge(ui, pal, &format!("❌ {failed}"), Tone::Danger);
                        theme::badge(ui, pal, &format!("⚠ {masked}"), Tone::Warning);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            // 走 worker 清空而不是本地 clear：
                            // 本地 clear 和「worker 刚发出的新结论」会打架，
                            // 点完清空卡片上立刻又冒出一条旧的（见 ToUi::VerifyCleared）。
                            if ui.add(theme::ghost_button("清除", pal)).clicked() {
                                let _ = self.tx.send(ToWorker::ClearVerifications);
                            }
                        });
                    });
                    ui.add_space(theme::SPACE_8);
                    egui::ScrollArea::vertical()
                        .id_salt("verify_scroll")
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            for (cmd, result, detail) in &self.verifications {
                                // ⚠ 是「无法确认」，既不是成功也不是失败 ——
                                // 报成功是假保证，报失败是假警报，如实说不知道。
                                let (mark, tone) = match result {
                                    VerifyResult::Confirmed => ("✅", Tone::Success),
                                    VerifyResult::Failed => ("❌", Tone::Danger),
                                    VerifyResult::Masked => ("⚠", Tone::Warning),
                                };
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(
                                        egui::RichText::new(mark)
                                            .size(theme::TEXT_BODY)
                                            .color(tone.fg(pal)),
                                    );
                                    ui.label(theme::mono(cmd.as_str(), pal.text_primary));
                                    ui.label(theme::mono(format!("— {detail}"), tone.fg(pal)));
                                });
                            }
                        });
                });
            });
    }

    // ---------- 底部命令条 ----------

    fn command_bar(&mut self, ui: &mut egui::Ui, pal: &Palette) {
        egui::Panel::bottom("command")
            .frame(
                egui::Frame::NONE.fill(pal.bg_base).inner_margin(egui::Margin::symmetric(
                    theme::SPACE_13 as i8,
                    theme::SPACE_8 as i8,
                )),
            )
            .show_separator_line(false)
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        egui::RichText::new(">")
                            .size(theme::TEXT_BODY)
                            .monospace()
                            .color(pal.accent),
                    );
                    let can = self.can_send();
                    let width = ui.available_width() - theme::SPACE_89;
                    let resp = ui.add_enabled(
                        can,
                        egui::TextEdit::singleline(&mut self.custom)
                            .hint_text("自定义命令，Ctrl+Enter 发送")
                            .font(egui::TextStyle::Monospace)
                            .desired_width(width.max(theme::SPACE_89)),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && can {
                        let c = self.custom.trim().to_string();
                        if !c.is_empty() {
                            let _ = self.tx.send(ToWorker::RunRaw { cmd: c });
                            self.custom.clear();
                        }
                    }
                    if ui.add_enabled(can, theme::primary_button("发送", pal)).clicked() {
                        let c = self.custom.trim().to_string();
                        if !c.is_empty() {
                            let _ = self.tx.send(ToWorker::RunRaw { cmd: c });
                            self.custom.clear();
                        }
                    }
                });
            });
    }

    // ---------- 参数表单（居中模态）----------

    fn param_modal(&mut self, ctx: &egui::Context, pal: &Palette) {
        if self.param_form.is_none() {
            return;
        }
        let (title, danger, note, error) = {
            let f = self.param_form.as_ref().unwrap();
            (f.title.clone(), f.danger, f.note.clone(), f.error.clone())
        };

        let mut do_submit = false;
        let mut do_cancel = false;

        egui::Modal::new(egui::Id::new("param_modal"))
            .frame(theme::modal_frame(pal))
            // 遮罩要和另外两个模态一致。egui 默认是 alpha 100（39%），
            // 三个模态弹出来深浅不一，用户会以为「这个弹窗是不是没画完」。
            .backdrop_color(egui::Color32::from_black_alpha(
                theme::MODAL_BACKDROP_ALPHA,
            ))
            .show(ctx, |ui| {
                ui.set_width(theme::MODAL_W_FORM);
                ui.horizontal(|ui| {
                    ui.label(theme::body(format!("填写参数 · {title}"), pal).strong());
                    if danger {
                        theme::badge(ui, pal, "危险", Tone::Danger);
                    }
                });
                if let Some(n) = &note {
                    ui.add_space(theme::SPACE_8);
                    theme::tone_frame(Tone::Danger, pal).show(ui, |ui| {
                        ui.label(theme::secondary(n, pal));
                    });
                }
                ui.add_space(theme::SPACE_13);

                if let Some(form) = self.param_form.as_mut() {
                    for (def, val) in form.fields.iter_mut() {
                        ui.label(theme::caption(
                            format!("{}  [{}]", def.name, def.kind.keyword()),
                            pal,
                        ));
                        let masked =
                            matches!(def.kind, zte_g7615_panel::config::ParamKind::Password);
                        ui.add(
                            egui::TextEdit::singleline(val)
                                .password(masked)
                                .hint_text(def.hint.clone().unwrap_or_default())
                                .desired_width(f32::INFINITY),
                        );
                        ui.add_space(theme::SPACE_8);
                    }
                }

                if let Some(e) = &error {
                    ui.add_space(theme::SPACE_4);
                    ui.label(
                        egui::RichText::new(e)
                            .size(theme::TEXT_BODY)
                            .color(pal.danger),
                    );
                }

                ui.add_space(theme::SPACE_21);
                ui.horizontal(|ui| {
                    if ui.add(theme::primary_button("执行", pal)).clicked() {
                        do_submit = true;
                    }
                    if ui.add(theme::ghost_button("取消", pal)).clicked() {
                        do_cancel = true;
                    }
                    if danger {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(theme::caption("执行后还需二次确认", pal));
                        });
                    }
                });
            });

        if do_cancel {
            self.param_form = None;
        }
        if do_submit {
            self.submit_param_form();
        }
    }

    // ---------- 危险操作二次确认（居中模态）----------

    fn danger_modal(&mut self, ctx: &egui::Context, pal: &Palette) {
        if self.pending.is_none() {
            return;
        }
        let (title, note) = {
            let (t, _, n) = self.pending.as_ref().unwrap();
            (t.clone(), n.clone())
        };

        let mut confirm = false;
        let mut cancel = false;

        egui::Modal::new(egui::Id::new("danger_modal"))
            .frame(theme::modal_frame(pal))
            .backdrop_color(egui::Color32::from_black_alpha(
                theme::MODAL_BACKDROP_ALPHA,
            ))
            .show(ctx, |ui| {
                ui.set_width(theme::MODAL_W_DANGER);
                ui.horizontal(|ui| {
                    theme::badge(ui, pal, "危险操作", Tone::Danger);
                    ui.label(theme::body(&title, pal).strong());
                });
                ui.add_space(theme::SPACE_13);
                theme::tone_frame(Tone::Danger, pal).show(ui, |ui| {
                    ui.label(theme::secondary(
                        "该操作会修改光猫配置，且立即生效、没有撤销键。",
                        pal,
                    ));
                    if let Some(n) = &note {
                        ui.add_space(theme::SPACE_4);
                        ui.label(theme::secondary(n, pal));
                    }
                });
                ui.add_space(theme::SPACE_21);
                ui.horizontal(|ui| {
                    if ui.add(theme::danger_button("确认执行", pal)).clicked() {
                        confirm = true;
                    }
                    if ui.add(theme::ghost_button("取消", pal)).clicked() {
                        cancel = true;
                    }
                });
            });

        if confirm {
            if let Some((title, cmds, _)) = self.pending.take() {
                // 危险操作同样要展开占位符，否则会把 __SET_TIME_NOW__ 发到设备
                let _ = self.tx.send(ToWorker::Run {
                    title,
                    cmds: expand_placeholders(&cmds),
                });
            }
        }
        if cancel {
            self.pending = None;
        }
    }

    // ---------- 刷机级确认（改区域专用）----------

    /// 进入刷机级闸门。参数从预设里取，区域码单独存在表单里。
    fn begin_critical(&mut self, entry: (String, Vec<String>, Option<String>)) {
        let (title, cmds, note) = entry;
        self.critical = Some(CriticalForm::new(title, cmds, note));
    }

    /// 「① 备份并检查」：先把设备原值读全并写盘，再去读当前区域码。
    ///
    /// ## 为什么必须让 worker 单独处理（v1.2 修复的严重缺陷）
    ///
    /// 旧实现只把「即将执行什么」写进备份文件 —— 那**等于没备份**：
    /// `upgradetest sdefconf` 会把配置重置成省模板，而备份里一个字都
    /// 没有设备原值，用户拿着一份命令清单救不回任何东西。
    ///
    /// 旧实现还顺带复用了 `ToWorker::Run`，而 `Run` 前置的
    /// `backup_tables_before_write` 只对 `sendcmd 1 DB set` 推导得出
    /// 表名，纯 `cat` 命令推导结果为空，于是整个备份被跳过。
    ///
    /// 更糟的是 `Run` 的备份失败**不阻断**（这是普通写操作的合理取舍），
    /// 而改区域不可逆，绝不能接受「备份没成功也放行」。
    ///
    /// 所以改成专用消息 `CriticalBackup`：由 worker 明确回报成功/失败，
    /// 只有拿到真实路径才把 `backup_path` 填上，闸门才解锁下一步。
    fn critical_backup(&mut self) {
        // 先把要用的东西拷出来再改 self：
        // form 借的是 self.critical，append_log 借的是 self 整体，
        // 同时持有会直接编译不过（E0499）。
        let Some(form) = self.critical.as_ref() else {
            return;
        };
        let tables: Vec<String> = Self::CRITICAL_BACKUP_TABLES
            .iter()
            .map(|s| s.to_string())
            .collect();
        let title = form.title.clone();
        let probe_cmds = form.probe_cmds.clone();
        let planned_cmds = form.cmds.clone();

        // 领一个新序号：本轮结论回来时靠它和上一轮区分开。
        self.critical_backup_seq = self.critical_backup_seq.wrapping_add(1);
        let token = self.critical_backup_seq;

        // 先清掉上一轮的结论：否则界面上会残留「已备份」，
        // 而这一轮其实还没开始。
        if let Some(f) = self.critical.as_mut() {
            f.backup_path = None;
            f.backup_error = None;
            f.backup_tables.clear();
            f.backup_token = token;
        }
        self.append_log(concat!(
            "[区域闸门] 开始备份：先读取关键表原值，再读当前区域与串码…\n",
            "[区域闸门] 备份未成功前，第 ②③ 步保持锁定。\n"
        ));
        // 置 busy：按钮在备份跑完前必须禁用，否则连点会压两条消息进通道。
        if let Some(f) = self.critical.as_mut() {
            f.busy = true;
        }
        let _ = self.tx.send(ToWorker::CriticalBackup {
            token,
            title: format!("区域闸门-{title}"),
            tables,
            probe_cmds,
            planned_cmds,
        });
    }

    fn critical_modal(&mut self, ctx: &egui::Context, pal: &Palette) {
        if self.critical.is_none() {
            return;
        }

        let detected = self.detected_model;
        let connected = self.connected;
        let log_snapshot = self.log.clone();

        let mut do_backup = false;
        let mut do_execute = false;
        let mut do_cancel = false;

        {
            let form = self.critical.as_mut().unwrap();
            form.current_detected = CriticalForm::parse_region(&log_snapshot);

            egui::Modal::new(egui::Id::new("critical_modal"))
                .frame(theme::modal_frame(pal))
                .backdrop_color(egui::Color32::from_black_alpha(
                    theme::MODAL_BACKDROP_ALPHA,
                ))
                .show(ctx, |ui| {
                    ui.set_width(theme::MODAL_W_CRITICAL);

                    ui.horizontal(|ui| {
                        theme::badge(ui, pal, "刷机级操作", Tone::Danger);
                        ui.label(theme::body(&form.title, pal).strong());
                    });
                    ui.add_space(theme::SPACE_8);

                    if detected.is_none() {
                        theme::tone_frame(Tone::Warning, pal).show(ui, |ui| {
                            ui.label(theme::secondary(
                                "机型未识别，无法确认兼容性。已知部分新固件与联通 7015UV5 执行本操作会写入失败。",
                                pal,
                            ));
                        });
                        ui.add_space(theme::SPACE_8);
                    }

                    theme::tone_frame(Tone::Danger, pal).show(ui, |ui| {
                        ui.label(theme::secondary(
                            "执行后设备会立即重启，配置重置为该省默认模板：超密回默认、WAN 需重建、Telnet 必掉，可能需要重新注册（LOID/SN）。",
                            pal,
                        ));
                        if let Some(n) = &form.note {
                            ui.add_space(theme::SPACE_4);
                            ui.label(theme::secondary(n, pal));
                        }
                    });

                    ui.add_space(theme::SPACE_13);

                    // ---- ① 备份 ----
                    ui.horizontal(|ui| {
                        ui.label(theme::body("① 备份并读取当前状态", pal).strong());
                        if ui
                            .add_enabled(
                                connected && !form.busy,
                                theme::ghost_button("备份并检查", pal),
                            )
                            .clicked()
                        {
                            do_backup = true;
                        }
                        if form.backup_path.is_some() {
                            // 说清备了几张，而不是笼统一句「已备份」：
                            // 只读到 1 张表也叫「已备份」，那份文件救不回设备。
                            ui.label(theme::caption(
                                format!(
                                    "已备份（{} 张表）",
                                    form.backup_tables.len()
                                ),
                                pal,
                            ));
                        }
                    });
                    if let Some(e) = &form.backup_error {
                        ui.label(
                            egui::RichText::new(e)
                                .size(theme::TEXT_CAPTION)
                                .color(pal.danger),
                        );
                    }
                    if let Some(d) = &form.current_detected {
                        ui.label(theme::caption(format!("设备当前区域码：{d}"), pal));
                    } else if form.backup_path.is_some() {
                        ui.label(theme::caption(
                            "未能从输出里读到区域码，请确认设备支持 cat /userconfig/flag_type",
                            pal,
                        ));
                    }

                    ui.add_space(theme::SPACE_13);

                    // ---- ② 三勾选 ----
                    ui.label(theme::body("② 逐条确认", pal).strong());
                    let ready = form.backup_ok();
                    // add_enabled_ui 只关**交互**，不会把文字调暗。
                    // 备份前这三条看起来和可用时一模一样，点了没反应 ——
                    // 用户只会以为界面卡住了。所以这里显式换弱色 + 说明原因。
                    let ack_color = if ready {
                        pal.text_primary
                    } else {
                        pal.text_muted
                    };
                    if !ready {
                        ui.label(theme::caption(
                            "完成第 ① 步「备份并检查」后才能勾选",
                            pal,
                        ));
                    }
                    ui.add_enabled_ui(ready, |ui| {
                        ui.checkbox(
                            &mut form.ack_reboot,
                            egui::RichText::new("我知道执行后设备会立即重启").color(ack_color),
                        );
                        ui.checkbox(
                            &mut form.ack_config,
                            egui::RichText::new(
                                "我知道配置会重置为该省模板，Telnet 会掉，需要重新开通",
                            )
                            .color(ack_color),
                        );
                        ui.checkbox(
                            &mut form.ack_register,
                            egui::RichText::new(
                                "我知道可能需要重新注册（LOID/SN），且改区域不等于换运营商界面",
                            )
                            .color(ack_color),
                        );
                    });

                    ui.add_space(theme::SPACE_13);

                    // ---- ③ 双输入 ----
                    ui.label(theme::body("③ 输入区域码", pal).strong());
                    ui.add_enabled_ui(ready, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(theme::caption("当前区域码", pal));
                            ui.add_sized(
                                egui::vec2(theme::FIELD_W_PORT, theme::CONTROL_HEIGHT),
                                theme::centered_edit(&mut form.current_input, theme::FIELD_W_PORT, Some("如 305")),
                            );
                            if let Some(d) = &form.current_detected {
                                ui.label(theme::caption(format!("（设备读到 {d}）"), pal));
                            }
                        });
                        ui.add_space(theme::SPACE_4);
                        ui.horizontal(|ui| {
                            ui.label(theme::caption("目标区域码", pal));
                            ui.add_sized(
                                egui::vec2(theme::FIELD_W_PORT, theme::CONTROL_HEIGHT),
                                theme::centered_edit(&mut form.target_input, theme::FIELD_W_PORT, Some("如 210")),
                            );
                            ui.label(theme::caption("再输一次", pal));
                            ui.add_sized(
                                egui::vec2(theme::FIELD_W_PORT, theme::CONTROL_HEIGHT),
                                theme::centered_edit(&mut form.target_confirm, theme::FIELD_W_PORT, Some("重复目标码")),
                            );
                        });
                    });

                    // 输入一填完就即时反馈，别等点了「确认执行」才说「两次不一致」。
                    // 点击前报错是被动告知，边输边报才是防止误操作。
                    // 只在「已经能填」之后提示，否则一打开闸门就满屏红字。
                    if form.error.is_none() && ready {
                        if let Err(e) = form.validate() {
                            ui.add_space(theme::SPACE_8);
                            ui.label(theme::caption(e, pal));
                        }
                    }
                    if let Some(e) = &form.error {
                        ui.add_space(theme::SPACE_8);
                        theme::tone_frame(Tone::Danger, pal).show(ui, |ui| {
                            ui.label(
                                egui::RichText::new(e)
                                    .size(theme::TEXT_BODY)
                                    .color(pal.danger),
                            );
                        });
                    }

                    ui.add_space(theme::SPACE_13);

                    // ---- 翻车了怎么办 ----
                    let (_, open) = theme::collapsible_header(
                        ui,
                        pal,
                        ui.make_persistent_id("critical_faq"),
                        "翻车了怎么办",
                        None,
                        false,
                    );
                    if open {
                        ui.indent("critical_faq_body", |ui| {
                            ui.label(theme::secondary(
                                "区域变成「未知」：多为固件不认该区域码，需要 TTL 清 usercfg 分区后重刷。",
                                pal,
                            ));
                            ui.label(theme::secondary(
                                "Telnet 掉了：改区域必然丢 Telnet，用原来的开 Telnet 工具重开；新固件免费工具可能失效。",
                                pal,
                            ));
                            ui.label(theme::secondary(
                                "卡在注册 / O3：需要装维在 OLT 侧解绑旧设备，或重新下发业务。",
                                pal,
                            ));
                            ui.add_space(theme::SPACE_4);
                            ui.label(theme::caption(
                                "以上处置方向来自论坛经验，没有官方文档；本程序不提供自动修复。",
                                pal,
                            ));
                        });
                    }

                    ui.add_space(theme::SPACE_21);

                    let can_exec = form.can_execute();
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(can_exec, theme::danger_button("确认执行", pal))
                            .clicked()
                        {
                            do_execute = true;
                        }
                        if ui.add(theme::ghost_button("取消", pal)).clicked() {
                            do_cancel = true;
                        }
                        if !can_exec {
                            ui.label(theme::caption("完成 ①②③ 后才能执行", pal));
                        }
                    });
                });
        }

        if do_backup {
            self.critical_backup();
        }
        if do_execute {
            self.execute_critical();
        }
        if do_cancel {
            self.critical = None;
        }
    }

    /// 真正下发改区域命令。
    ///
    /// 执行前**再校验一次**：界面上的按钮禁用只是第一道，
    /// 逻辑层必须能独立拦住，否则以后加个快捷键就绕过去了。
    fn execute_critical(&mut self) {
        let Some(form) = self.critical.as_mut() else {
            return;
        };
        if let Err(e) = form.validate() {
            form.error = Some(e);
            return;
        }
        let code = form.target_input.trim().to_string();
        let title = form.title.clone();
        let cmds: Vec<String> = form
            .cmds
            .iter()
            .map(|c| c.replace("$目标区域码", &code))
            .collect();

        self.critical = None;
        self.append_log(&format!(
            "\n[区域闸门] 已确认执行：目标区域码 {code}。设备将立即重启，Telnet 会断开。\n\
             [区域闸门] 重启后需要：重新开 Telnet → 重新配置 WAN → 重新注册。\n"
        ));
        let _ = self.tx.send(ToWorker::Run { title, cmds });
    }
}

#[cfg(test)]
mod tests {
    use super::{
        backup_result_is_stale, can_send_gate, is_safe_table_name, line_wraps, route, LogKind,
        Route,
    };

    /// 折行判断必须偏保守：判窄了会让滚到底时末尾被切掉。
    ///
    /// `show_rows` 的前提是每行等高，一条折成两行的日志会让整段
    /// 高度被低估，于是最后几行被画到可视区外。
    #[test]
    fn 折行判断按半角宽度估算() {
        // 10 个半角字符，一行能放 10 个 → 正好不折
        assert!(!line_wraps("0123456789", 10));
        // 11 个 → 折
        assert!(line_wraps("01234567890", 10));
        // 中文按两格算：4 个汉字 = 8 格，不折
        assert!(!line_wraps("中文日志", 10));
        // 5 个汉字 = 10 格，正好不折
        assert!(!line_wraps("中文日志测", 10));
        // 6 个汉字 = 12 格 > 10 → 折
        assert!(line_wraps("中文日志测试", 10));
        // 未知宽度（usize::MAX）一律判不折，走虚拟化快路径
        assert!(!line_wraps(&"长".repeat(10_000), usize::MAX));
        // 空行不折
        assert!(!line_wraps("", 1));
    }

    /// 模态开着时**任何**发命令的路径都必须关掉。
    ///
    /// 模态的遮罩只挡鼠标。命令条的输入框仍可编辑、Ctrl+Enter 仍被
    /// `handle_shortcuts` 读原始键盘事件，所以闸门开着的时候按一下
    /// 就能把命令发出去 —— 备份与三段确认全被绕过。
    #[test]
    fn 模态开着时不许发命令() {
        // 正常可用
        assert!(can_send_gate(true, false, false, false));
        // 四个条件任缺其一都不许发
        assert!(!can_send_gate(false, false, false, false), "未连接不该发");
        assert!(!can_send_gate(true, true, false, false), "忙时不该发");
        assert!(!can_send_gate(true, false, true, false), "worker 已死不该发");
        assert!(
            !can_send_gate(true, false, false, true),
            "模态开着（危险确认/参数表单/区域闸门）时不该发"
        );
        // 全都不满足时当然也不许发
        assert!(!can_send_gate(false, true, true, true));
    }

    /// 过期的备份结论必须被丢弃。
    ///
    /// 场景：点「备份并检查」→ 立刻取消 → 重新打开闸门。
    /// 这时 worker 那条备份才跑完并回传，如果不比对序号，
    /// 它会把**新表单**填成「已备份」，第 ① 步在当前这次打开里
    /// 一次都没跑过，②③ 却被解锁了。
    #[test]
    fn 过期的备份结论被丢弃() {
        // 序号一致 → 有效
        assert!(!backup_result_is_stale(Some(7), 7));
        // 序号不一致 → 上一轮的，丢弃
        assert!(backup_result_is_stale(Some(7), 6));
        assert!(backup_result_is_stale(Some(8), 7));
        // 闸门已经关掉 → 无处安放，一律丢弃
        assert!(backup_result_is_stale(None, 7));
        // 序号 0 是初始值，不能当成「没有序号」而误判
        assert!(!backup_result_is_stale(Some(0), 0));
    }


    /// 表名是用户直接输入的，拼进命令前必须挡住换行与分号。
    ///
    /// `Telnet::send_line` 只加 `\r\n` 写出，不过滤内容，所以
    /// 表名里塞 `\r\nreboot` 就等于替用户多执行一条 reboot
    /// —— 界面上显示的是「读表」，设备却重启了。
    #[test]
    fn 表名必须挡住命令注入() {
        // 正常表名全放行
        for ok in ["DevInfo", "WLANCfg", "PONINFO", "DevAuthInfo", "A_1"] {
            assert!(is_safe_table_name(ok), "{ok} 应该是合法表名");
        }
        // 会被 send_line 当成第二条命令的输入
        for bad in [
            "DevInfo\r\nreboot",
            "DevInfo\nreboot",
            "DevInfo\rreboot",
            "DevInfo ; reboot",
            "DevInfo;reboot",
            "DevInfo reboot",
            "",
            "表名",
        ] {
            assert!(!is_safe_table_name(bad), "{bad:?} 必须被拒绝");
        }
    }

    /// 验收第 9 条：`修改区域码` 必须是 critical，且**不进**普通危险确认。
    ///
    /// 这条测试锁死的是「以后有人把它改回 danger」这种退化 ——
    /// 改回 danger 之后闸门就没了，改区域变成两次点击就能执行。
    #[test]
    fn 修改区域码走专用闸门而不是普通危险确认() {
        let groups = zte_g7615_panel::presets::builtin();
        let target = groups
            .iter()
            .flat_map(|(_, actions)| actions.iter())
            .find(|a| a.name.contains("修改区域码"))
            .expect("内置清单里必须有「修改区域码」");
        assert!(target.critical, "修改区域码必须是 critical");
        assert_eq!(route(target), Route::Critical, "必须路由到专用闸门");
        assert_ne!(route(target), Route::DangerConfirm);
        assert_ne!(route(target), Route::Direct);
        assert_ne!(route(target), Route::ParamForm);
    }

    /// critical 即使带参数也不能落进普通参数表单 ——
    /// 那样用户填完点「执行」就把备份和三段确认全绕过了。
    #[test]
    fn critical带参数也不走参数表单() {
        let t = "group G\n  action 改区 | critical | 说明\n    param 目标区域码 | region\n    cmd upgradetest sdefconf $目标区域码\n";
        let groups = zte_g7615_panel::presets::parse_str(t).unwrap();
        let a = &groups[0].1[0];
        assert!(a.needs_input(), "这个用例本身要带参数");
        assert_eq!(route(a), Route::Critical);
    }

    /// 危险操作**绝不能**走 Direct —— 这是二次确认不可跳过的机器可验证版本。
    ///
    /// 用内置清单（不读磁盘）跑，所以结果与用户改没改预设文件无关。
    #[test]
    fn 危险操作永远不会被直接执行() {
        let groups = zte_g7615_panel::presets::builtin();
        let mut total_danger = 0;
        let mut direct = Vec::new();
        for (_, actions) in &groups {
            for a in actions {
                if a.danger {
                    total_danger += 1;
                    if route(a) == Route::Direct {
                        direct.push(a.name.clone());
                    }
                }
            }
        }
        assert!(total_danger > 0, "内置清单里应该有危险操作");
        assert!(
            direct.is_empty(),
            "这些危险操作会绕过二次确认直接执行：{direct:?}"
        );
    }

    /// 需要填参数的操作必须先弹表单，不能把占位参数直接发到设备。
    ///
    /// 刷机级（critical）是**唯一例外**：它同样带参数，但必须进专用闸门 ——
    /// 走普通表单的话，填完点「执行」就绕过了备份与三段确认。
    #[test]
    fn 需要参数的操作先弹表单() {
        let groups = zte_g7615_panel::presets::builtin();
        for (_, actions) in &groups {
            for a in actions {
                if a.critical {
                    continue; // 专用闸门，另有单测覆盖
                }
                if a.needs_input() {
                    assert_eq!(route(a), Route::ParamForm, "{} 应先弹参数表单", a.name);
                }
            }
        }
    }

    /// 普通只读操作直接执行，不该给用户加一道无意义的确认。
    #[test]
    fn 只读操作直接执行() {
        let groups = zte_g7615_panel::presets::builtin();
        let mut checked = 0;
        for (_, actions) in &groups {
            for a in actions {
                if !a.danger && !a.needs_input() {
                    assert_eq!(route(a), Route::Direct, "{} 不该被拦", a.name);
                    checked += 1;
                }
            }
        }
        assert!(checked > 0, "内置清单里应该有只读操作");
    }

    /// 分类顺序是这条测试的核心：
    /// 「回读校验未通过」必须先被判成 Error，而不是被分隔线规则吃掉。
    #[test]
    fn 日志行分类优先识别关键结论() {
        assert_eq!(
            LogKind::of("----- 回读校验：1/1 条写命令已确认生效 -----"),
            LogKind::Success
        );
        assert_eq!(
            LogKind::of("----- 注意：1 条写命令未通过回读校验，配置可能没有真正写入 -----"),
            LogKind::Error
        );
        assert_eq!(LogKind::of("----- 完成 -----"), LogKind::Rule);
        assert_eq!(LogKind::of("===== 设备信息 ====="), LogKind::Heading);
        assert_eq!(LogKind::of("> sendcmd 1 DB p DevInfo"), LogKind::Command);
        assert_eq!(LogKind::of("[错误] 尚未连接"), LogKind::Error);
        assert_eq!(LogKind::of("[已就绪]"), LogKind::Success);
        assert_eq!(LogKind::of("[机型] 已隐藏 3 条命令"), LogKind::Notice);
        assert_eq!(LogKind::of("[超时] 命令未返回"), LogKind::Warning);
        assert_eq!(LogKind::of("随便一行输出"), LogKind::Plain);
        assert_eq!(LogKind::of("   "), LogKind::Plain);
    }
}
