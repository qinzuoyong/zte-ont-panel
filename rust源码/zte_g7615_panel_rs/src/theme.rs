//! 设计令牌与主题安装。
//!
//! 这个模块是界面层**唯一**允许出现颜色、字号、间距数字的地方。
//! `main.rs` 里的布局代码只引用这里的常量与构造函数 ——
//! 否则「统一改一下间距」会变成全文搜索替换的体力活。
//!
//! 取值依据见 `docs/设计规范.md`：
//! - 间距是斐波那契数列（比值收敛到 φ）
//! - 字号按 φ 逐级放大：12 / 13 / 21 / 34
//! - 侧栏宽 = 窗口宽 / φ³，窗口高 = 宽 / φ
//!
//! 只做视觉，不碰任何业务逻辑：这里没有一个函数会发命令或读设备。

// 主题层是「先定义好整套令牌，界面按需取用」：
// 有些令牌（如 section_title、Tone::Accent）当前用不到，
// 但缺了它们，下次加组件又会退回到随手写死数字。
// 所以整个模块允许存在暂时未被引用的公开项。
#![allow(dead_code)]

use eframe::egui::{
    self, Color32, CornerRadius, FontFamily, FontId, Margin, Shadow, Stroke, TextStyle,
    ThemePreference, Visuals,
};

// ---------------- 间距刻度 ----------------
// 2, 4, 8, 13, 21, 34, 55, 89 —— 相邻比值从第 3 项起收敛到 1.618。

pub const SPACE_2: f32 = 2.0;
pub const SPACE_4: f32 = 4.0;
pub const SPACE_8: f32 = 8.0;
pub const SPACE_13: f32 = 13.0;
pub const SPACE_21: f32 = 21.0;
pub const SPACE_34: f32 = 34.0;
pub const SPACE_55: f32 = 55.0;
pub const SPACE_89: f32 = 89.0;

// ---------------- 字号 ----------------
pub const TEXT_CAPTION: f32 = 12.0;
pub const TEXT_BODY: f32 = 13.0;
pub const TEXT_SECTION: f32 = 21.0;
pub const TEXT_DISPLAY: f32 = 34.0;

/// 正文与日志的行高：13 × 1.618 = 21.03 → 21。
pub const LINE_HEIGHT: f32 = 21.0;

// ---------------- 圆角 ----------------
pub const RADIUS_CHIP: u8 = 4;
pub const RADIUS_CONTROL: u8 = 8;
pub const RADIUS_CARD: u8 = 13;

// ---------------- 尺寸（全部由 φ 推导） ----------------
/// 窗口默认 1280 × 792（1280 / φ = 791.1）。
pub const WINDOW_DEFAULT_W: f32 = 1280.0;
pub const WINDOW_DEFAULT_H: f32 = 792.0;
/// 窗口最小 1000 × 618（1000 / φ = 618.0）。
pub const WINDOW_MIN_W: f32 = 1000.0;
pub const WINDOW_MIN_H: f32 = 618.0;

/// 侧栏默认宽 = 1280 / φ³ = 302.1。
pub const SIDEBAR_DEFAULT: f32 = 302.0;
/// 侧栏最窄 = 302 / φ = 186.7。
pub const SIDEBAR_MIN: f32 = 187.0;
/// 侧栏最宽 = 302 × φ = 488.6。
pub const SIDEBAR_MAX: f32 = 489.0;
/// 低于这个宽度进「图标模式」：只显示首字 + tooltip。
pub const SIDEBAR_ICON_BREAKPOINT: f32 = 233.0;

/// 顶栏高度。
pub const TOPBAR_HEIGHT: f32 = SPACE_89 + SPACE_13;

// ---- 由刻度派生出的具体尺寸 ----
// 放在这里而不是散在 ui.rs：验收要求「布局代码里不许出现刻度外的裸数字」，
// 把这些「89 的几倍」也收进令牌层，ui.rs 里就只剩 SPACE_* 与 0.0。

/// 地址 / 账号输入框宽度。
pub const FIELD_W: f32 = SPACE_89;
/// 密码框比地址框宽一点：密码通常更长，太窄会一直横向滚动。
pub const FIELD_W_PWD: f32 = SPACE_89 + SPACE_21;
/// 端口框：Telnet 端口最多 5 位，55 够用。
pub const FIELD_W_PORT: f32 = SPACE_55;
/// 顶栏连接胶囊的最小可用宽度，低于它就整段不画（否则会压到输入框）。
pub const CAPSULE_MIN_W: f32 = SPACE_89 * 3.0;
/// 回读校验面板默认高度。
pub const VERIFY_PANEL_H: f32 = SPACE_89 + SPACE_55;
/// 回读校验最多保留的条数。
pub const VERIFY_HISTORY_MAX: usize = 50;
/// 参数表单模态宽度。
pub const MODAL_W_FORM: f32 = SPACE_89 * 5.0;
/// 危险确认模态宽度（比表单窄一档：它只有一段说明和两个按钮）。
pub const MODAL_W_DANGER: f32 = SPACE_89 * 4.0;
/// 刷机级确认模态宽度。
///
/// 比普通危险确认宽得多：它要装下三行勾选、三个输入框和一块折叠的
/// 「翻车了怎么办」，挤在 356 里会全部换行，反而看不清。
pub const MODAL_W_CRITICAL: f32 = SPACE_89 * 7.0;
/// 模态遮罩的不透明度。
pub const MODAL_BACKDROP_ALPHA: u8 = 180;
/// busy / connecting 时的重绘间隔（毫秒）。
pub const REPAINT_INTERVAL_MS: u64 = 100;
/// 主按钮高度。
pub const CONTROL_HEIGHT: f32 = SPACE_34;
/// 图标按钮宽度。
pub const ICON_BUTTON_W: f32 = SPACE_55;

/// 危险操作左侧竖条的宽度。
///
/// 3 不在斐波那契刻度上，这是刻意的：它是**描边宽度**而不是间距，
/// 间距管「两块内容隔多远」，描边管「这条线多粗」。
/// 3px 是 1px 分隔线的 3 倍，在 1x/1.5x/2x 缩放下都不会糊成发丝。
pub const STRIPE_WIDTH: f32 = 3.0;

// ---------------- 色板 ----------------
const fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb(
        ((hex >> 16) & 0xFF) as u8,
        ((hex >> 8) & 0xFF) as u8,
        (hex & 0xFF) as u8,
    )
}

/// 一套完整配色。
///
/// 之前这里是 18 个散落的 `pub const`，只够描述暗色一套。
/// 支持明暗双主题之后，**同一时刻只能有一套生效**，
/// 散落常量没法表达「这是哪一套」，所以收进结构体，
/// 由 `palette(dark_mode)` 按当前主题取。
///
/// 字段顺序与 `docs/设计规范.md` 第 4 节色板表一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Palette {
    // 表面：从最底层到最上层
    pub bg_base: Color32,
    pub bg_panel: Color32,
    pub bg_raised: Color32,
    pub border_weak: Color32,
    pub border_strong: Color32,
    // 文字：从最强到最弱
    pub text_primary: Color32,
    pub text_secondary: Color32,
    pub text_muted: Color32,
    // 强调
    pub accent: Color32,
    pub accent_hover: Color32,
    // 语义
    pub success: Color32,
    pub danger: Color32,
    pub warning: Color32,
    pub info: Color32,
    // 语义底色（徽标 / 横幅的浅色块）
    pub danger_bg: Color32,
    pub success_bg: Color32,
    pub warning_bg: Color32,
    pub info_bg: Color32,
}

/// 暗色配色 —— **这 18 个值自 v1.0 起一个字节都没动过**，
/// 验收要求逐值对比一致（见 `docs/设计规范.md` 第 5 节实测对比度）。
pub const DARK: Palette = Palette {
    bg_base: rgb(0x0E1116),
    bg_panel: rgb(0x151A21),
    bg_raised: rgb(0x1C232C),
    border_weak: rgb(0x2A333F),
    border_strong: rgb(0x3A4655),

    text_primary: rgb(0xE6EDF3),
    text_secondary: rgb(0x9BA9B8),
    text_muted: rgb(0x6B7A8C),

    accent: rgb(0x4CC2FF),
    accent_hover: rgb(0x6FD0FF),

    success: rgb(0x3FB950),
    danger: rgb(0xF85149),
    warning: rgb(0xD29922),
    info: rgb(0x58A6FF),

    danger_bg: rgb(0x2D1417),
    success_bg: rgb(0x0F2417),
    warning_bg: rgb(0x2A2113),
    info_bg: rgb(0x12233A),
};

/// 浅色配色（v1.1 新增）。
///
/// 不是把暗色取反 —— 深色语义色靠「高亮度 + 高饱和」在暗底上取胜，
/// 搬到白底上会直接糊掉。这里每个值都按 WCAG 2.1 相对亮度公式实算过，
/// 对 `#FFFFFF` 与 `#F6F8FA` 两种表面都 ≥ 4.5:1（弱文字 ≥ 5.5:1，留足余量）。
///
/// 具体数值与实测对比度见 `docs/设计规范.md` 第 4.2 节。
pub const LIGHT: Palette = Palette {
    bg_base: rgb(0xFFFFFF),
    bg_panel: rgb(0xF6F8FA),
    bg_raised: rgb(0xFFFFFF),
    border_weak: rgb(0xD0D7DE),
    border_strong: rgb(0x8C959F),

    text_primary: rgb(0x1F2328),   // 15.80 / 14.84
    text_secondary: rgb(0x4E5862), // 7.25 / 6.81
    text_muted: rgb(0x5C6670),     // 5.85 / 5.50

    accent: rgb(0x0969DA),       // 5.19 / 4.88
    accent_hover: rgb(0x0550AE), // 7.59 / 7.13

    success: rgb(0x1A7F37), // 5.08 / 4.77
    danger: rgb(0xCF222E),  // 5.36 / 5.03
    warning: rgb(0x9A6700), // 4.87 / 4.57
    info: rgb(0x0969DA),    // 5.19 / 4.88

    danger_bg: rgb(0xFFEBE9),
    success_bg: rgb(0xDAFBE1),
    warning_bg: rgb(0xFFF8C5),
    info_bg: rgb(0xDDF4FF),
};

/// 按当前是否暗色取配色。
///
/// 界面代码一律写 `let pal = theme::palette(ui.visuals().dark_mode);`，
/// **不要**再引用具体某一套常量 —— 那样切主题时就会有一半控件不跟着变。
pub fn palette(dark_mode: bool) -> &'static Palette {
    if dark_mode {
        &DARK
    } else {
        &LIGHT
    }
}

/// disabled 控件的不透明度：灰掉文字会连「这是什么控件」都看不清，
/// 降透明度能保留形状。
pub const DISABLED_ALPHA: u8 = 102; // 40%

// ---------------- 语义徽标 ----------------

/// 徽标色调。三态校验、危险标记、机型标记都用它。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Neutral,
    Info,
    Success,
    Warning,
    Danger,
    Accent,
}

impl Tone {
    pub fn fg(self, pal: &Palette) -> Color32 {
        match self {
            Tone::Neutral => pal.text_secondary,
            Tone::Info => pal.info,
            Tone::Success => pal.success,
            Tone::Warning => pal.warning,
            Tone::Danger => pal.danger,
            Tone::Accent => pal.accent,
        }
    }

    pub fn bg(self, pal: &Palette) -> Color32 {
        match self {
            Tone::Neutral => pal.bg_raised,
            Tone::Info => pal.info_bg,
            Tone::Success => pal.success_bg,
            Tone::Warning => pal.warning_bg,
            Tone::Danger => pal.danger_bg,
            Tone::Accent => pal.bg_raised,
        }
    }
}

// ---------------- 主题安装 ----------------

/// 把令牌写进 egui 的全局样式。
///
/// 必须在 `App::new` 里调用一次：`Visuals` 是全局的，
/// 逐控件传颜色会让「改一套配色」变成改几十处。
///
/// `pref` 是用户的外观选择（浅色 / 深色 / 跟随系统）。
pub fn install(ctx: &egui::Context, pref: ThemePreference) {
    // ---- 关键坑位，改之前先读这段 ----
    //
    // egui 0.36 起 Style/Visuals 按明暗**两套**分别存。
    // 一定要用 set_visuals_of 把两套**都**写进去。
    //
    // 千万不要图省事写 ctx.set_visuals(v)：
    // 它的实现是 style_mut_of(self.theme(), ...)，只写**当前**那一套。
    // 启动时是暗色，于是浅色那套保持 egui 默认灰白；
    // 用户一切到浅色，本项目的配色就整体消失，界面变成默认 egui 脸。
    // 这个 bug 只在「切换之后」才看得见，静态截图查不出来。
    ctx.set_visuals_of(egui::Theme::Dark, visuals(true));
    ctx.set_visuals_of(egui::Theme::Light, visuals(false));

    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(SPACE_8, SPACE_8);
        style.spacing.button_padding = egui::vec2(SPACE_13, SPACE_4);
        style.spacing.interact_size = egui::vec2(SPACE_34, CONTROL_HEIGHT);
        style.spacing.window_margin = Margin::same(SPACE_13 as i8);
        style.spacing.menu_margin = Margin::same(SPACE_8 as i8);
        style.spacing.indent = SPACE_13;
        style.spacing.scroll.bar_width = SPACE_8;
        style.spacing.scroll.floating = true;

        style.text_styles = text_styles();

        // 悬停/选中的过渡时长：太短像闪烁，太长像卡顿。
        style.animation_time = 0.12;
    });

    // 系统主题探测失败时 egui 默认回落 Dark。
    // 本项目默认「跟随系统」，探测不到系统主题时也优先给浅色而不是深色：
    // egui 的兜底是 Dark，直接用它会在探测失败的机器上一开就黑。
    // 所以 fallback 仍然锁 Light，这是「跟随系统」的兜底，不是默认值本身。
    ctx.options_mut(|o| o.fallback_theme = egui::Theme::Light);

    ctx.set_theme(pref);
}

/// 把用户选择应用下去（切换主题时调用，立即生效，不需要重启）。
///
/// 只改偏好，不动已经装好的两套 Visuals —— 它们一直是就位的。
pub fn apply_theme(ctx: &egui::Context, pref: ThemePreference) {
    ctx.options_mut(|o| o.fallback_theme = egui::Theme::Light);
    ctx.set_theme(pref);
}

/// 字号表。四档，与 `docs/设计规范.md` 第 3 节一致。
pub fn text_styles() -> std::collections::BTreeMap<TextStyle, FontId> {
    use TextStyle::{Body, Button, Heading, Monospace, Small};
    let mut map = std::collections::BTreeMap::new();
    map.insert(Small, FontId::new(TEXT_CAPTION, FontFamily::Proportional));
    map.insert(Body, FontId::new(TEXT_BODY, FontFamily::Proportional));
    map.insert(Button, FontId::new(TEXT_BODY, FontFamily::Proportional));
    map.insert(Heading, FontId::new(TEXT_SECTION, FontFamily::Proportional));
    // 日志区：13px 等宽，行高由调用方按 LINE_HEIGHT 控制
    map.insert(Monospace, FontId::new(TEXT_BODY, FontFamily::Monospace));
    map
}

/// 按主题生成一套 Visuals。
///
/// `dark_mode` 为 true 时从 `Visuals::dark()` 派生，false 时从
/// `Visuals::light()` 派生 —— **不能都从 dark() 派生再改颜色**：
/// 两者的 `text_options.color_transfer_function` 不同（字体渲染伽马），
/// 浅色底上用暗色的伽马，字会发虚、发细，怎么调颜色都不对。
pub fn visuals(dark_mode: bool) -> Visuals {
    let pal = palette(dark_mode);
    let mut v = if dark_mode {
        Visuals::dark()
    } else {
        Visuals::light()
    };

    v.panel_fill = pal.bg_panel;
    v.window_fill = pal.bg_panel;
    v.extreme_bg_color = pal.bg_base;
    v.faint_bg_color = pal.bg_raised;
    v.window_stroke = Stroke::new(1.0, pal.border_weak);
    v.window_corner_radius = CornerRadius::same(RADIUS_CARD);
    // 两套都靠明度分层，不靠投影：投影一多就像网页弹窗
    v.window_shadow = Shadow::NONE;
    v.popup_shadow = Shadow::NONE;
    v.override_text_color = None;

    // disabled 用 40% 不透明度而不是把文字调灰：
    // 灰掉的文字连「这是什么控件」都看不清，降透明度能保留形状。
    v.disabled_alpha = DISABLED_ALPHA as f32 / 255.0;

    v.selection.bg_fill = pal.accent.gamma_multiply(0.35);
    v.selection.stroke = Stroke::new(1.0, pal.accent);

    // 非交互：文字、分隔线
    v.widgets.noninteractive.bg_fill = pal.bg_panel;
    v.widgets.noninteractive.weak_bg_fill = pal.bg_panel;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, pal.border_weak);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, pal.text_primary);
    v.widgets.noninteractive.corner_radius = CornerRadius::same(RADIUS_CONTROL);

    // 常态
    v.widgets.inactive.bg_fill = pal.bg_raised;
    v.widgets.inactive.weak_bg_fill = pal.bg_raised;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, pal.border_weak);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, pal.text_primary);
    v.widgets.inactive.corner_radius = CornerRadius::same(RADIUS_CONTROL);

    // 悬停
    // hover 用强调色描边而不是整块变色：
    // 整块变色会让「当前指到哪个」和「哪个被选中」分不清。
    v.widgets.hovered.bg_fill = pal.border_weak;
    v.widgets.hovered.weak_bg_fill = pal.border_weak;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, pal.accent_hover);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, pal.text_primary);
    v.widgets.hovered.corner_radius = CornerRadius::same(RADIUS_CONTROL);
    v.widgets.hovered.expansion = 0.0;

    // 按下
    v.widgets.active.bg_fill = pal.border_strong;
    v.widgets.active.weak_bg_fill = pal.border_strong;
    v.widgets.active.bg_stroke = Stroke::new(1.0, pal.accent);
    v.widgets.active.fg_stroke = Stroke::new(1.0, pal.text_primary);
    v.widgets.active.corner_radius = CornerRadius::same(RADIUS_CONTROL);
    v.widgets.active.expansion = 0.0;

    // 展开（下拉、折叠）
    v.widgets.open.bg_fill = pal.bg_raised;
    v.widgets.open.weak_bg_fill = pal.bg_raised;
    v.widgets.open.bg_stroke = Stroke::new(1.0, pal.border_strong);
    v.widgets.open.fg_stroke = Stroke::new(1.0, pal.text_primary);
    v.widgets.open.corner_radius = CornerRadius::same(RADIUS_CONTROL);

    v
}

// ---------------- 容器 ----------------

/// 顶栏：底色比面板更深一档，靠 1px 分隔线与内容区切开。
pub fn topbar_frame(pal: &Palette) -> egui::Frame {
    egui::Frame::NONE
        .fill(pal.bg_base)
        .inner_margin(Margin::symmetric(SPACE_21 as i8, SPACE_8 as i8))
}

/// 侧栏。
pub fn sidebar_frame(pal: &Palette) -> egui::Frame {
    egui::Frame::NONE
        .fill(pal.bg_panel)
        .inner_margin(Margin::symmetric(SPACE_13 as i8, SPACE_13 as i8))
}

/// 卡片：圆角 13 + 1px 弱边框，无投影。
pub fn card_frame(pal: &Palette) -> egui::Frame {
    egui::Frame::NONE
        .fill(pal.bg_raised)
        .stroke(Stroke::new(1.0, pal.border_weak))
        .corner_radius(CornerRadius::same(RADIUS_CARD))
        .inner_margin(Margin::same(SPACE_13 as i8))
}

/// 模态框：比卡片更亮一档，带一圈强边框把它从遮罩里拎出来。
pub fn modal_frame(pal: &Palette) -> egui::Frame {
    egui::Frame::NONE
        .fill(pal.bg_raised)
        .stroke(Stroke::new(1.0, pal.border_strong))
        .corner_radius(CornerRadius::same(RADIUS_CARD))
        .inner_margin(Margin::same(SPACE_21 as i8))
}

/// 语义底色块（危险横幅、校验摘要条）。
pub fn tone_frame(tone: Tone, pal: &Palette) -> egui::Frame {
    egui::Frame::NONE
        .fill(tone.bg(pal))
        .stroke(Stroke::new(1.0, tone.fg(pal).gamma_multiply(0.5)))
        .corner_radius(CornerRadius::same(RADIUS_CONTROL))
        .inner_margin(Margin::symmetric(SPACE_13 as i8, SPACE_8 as i8))
}

// ---------------- 文字 ----------------

pub fn caption(text: impl Into<String>, pal: &Palette) -> egui::RichText {
    egui::RichText::new(text).size(TEXT_CAPTION).color(pal.text_muted)
}

pub fn secondary(text: impl Into<String>, pal: &Palette) -> egui::RichText {
    egui::RichText::new(text).size(TEXT_BODY).color(pal.text_secondary)
}

pub fn body(text: impl Into<String>, pal: &Palette) -> egui::RichText {
    egui::RichText::new(text).size(TEXT_BODY).color(pal.text_primary)
}

pub fn section_title(text: impl Into<String>, pal: &Palette) -> egui::RichText {
    egui::RichText::new(text).size(TEXT_SECTION).color(pal.text_primary).strong()
}

pub fn display(text: impl Into<String>, pal: &Palette) -> egui::RichText {
    egui::RichText::new(text).size(TEXT_DISPLAY).color(pal.text_primary).strong()
}

/// 日志用：等宽 + 指定颜色，行高由 `LINE_HEIGHT` 控制。
pub fn mono(text: impl Into<String>, color: Color32) -> egui::RichText {
    egui::RichText::new(text)
        .monospace()
        .size(TEXT_BODY)
        .color(color)
}

// ---------------- 按钮 ----------------

/// 主按钮：强调色实心。
pub fn primary_button(text: impl Into<String>, pal: &Palette) -> egui::Button<'static> {
    // 文字用 bg_base：暗色下是「浅字压深底」，浅色下是「白字压蓝底」，
    // 两种都靠这同一个令牌自动成立 —— 实测白字对 #0969DA 是 5.19:1。
    egui::Button::new(
        egui::RichText::new(text.into())
            .size(TEXT_BODY)
            .color(pal.bg_base)
            .strong(),
    )
    .fill(pal.accent)
    .stroke(Stroke::NONE)
    .corner_radius(CornerRadius::same(RADIUS_CONTROL))
    .min_size(egui::vec2(0.0, CONTROL_HEIGHT))
}

/// 危险按钮：语义底色 + 同色描边。**不填满红色** ——
/// 满屏红会让「哪个才是真要命的操作」失去区分度。
pub fn danger_button(text: impl Into<String>, pal: &Palette) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text.into())
            .size(TEXT_BODY)
            .color(pal.danger)
            .strong(),
    )
    .fill(pal.danger_bg)
    .stroke(Stroke::new(1.0, pal.danger))
    .corner_radius(CornerRadius::same(RADIUS_CONTROL))
    .min_size(egui::vec2(0.0, CONTROL_HEIGHT))
}

/// 幽灵按钮：无填充，hover 才出底色。
pub fn ghost_button(text: impl Into<String>, pal: &Palette) -> egui::Button<'static> {
    egui::Button::new(
        egui::RichText::new(text.into())
            .size(TEXT_BODY)
            .color(pal.text_secondary),
    )
    .fill(Color32::TRANSPARENT)
    .stroke(Stroke::NONE)
    .corner_radius(CornerRadius::same(RADIUS_CONTROL))
    .min_size(egui::vec2(0.0, CONTROL_HEIGHT))
}

/// 侧栏里的操作行。
///
/// 不用 `egui::Button`：按钮的文字永远居中，而命令列表要的是**左对齐**——
/// 居中会让 55 行文字像一列锯齿，扫读时眼睛要来回找起点。
/// 这里自己分配整行矩形、自己画，左对齐、悬停高亮、危险标记全都可控。
pub fn action_row(
    ui: &mut egui::Ui,
    pal: &Palette,
    text: &str,
    danger: bool,
    enabled: bool,
) -> egui::Response {
    let width = ui.available_width();
    let sense = if enabled {
        egui::Sense::click()
    } else {
        egui::Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, CONTROL_HEIGHT), sense);

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if response.hovered() && enabled {
            painter.rect_filled(rect, CornerRadius::same(RADIUS_CHIP), pal.bg_raised);
        }
        // 键盘焦点环：自绘控件不会自动拿到 egui 的焦点高亮，
        // 不画的话 Tab 过来「看不见自己在哪一行」。
        if response.has_focus() {
            painter.rect_stroke(
                rect,
                CornerRadius::same(RADIUS_CHIP),
                Stroke::new(1.0, pal.accent),
                egui::StrokeKind::Inside,
            );
        }
        // 危险操作：左侧一条竖条。颜色之外再加形状线索，
        // 色弱用户也能一眼扫出哪几个会改配置。
        if danger {
            let stripe = egui::Rect::from_min_size(
                rect.left_top(),
                egui::vec2(STRIPE_WIDTH, rect.height()),
            );
            painter.rect_filled(stripe, CornerRadius::same(1), pal.danger);
        }

        // disabled 只降不透明度、不换颜色：
        // 换成灰色再降一次透明度就是「双重变暗」，文字会糊到读不出。
        let base = if danger { pal.danger } else { pal.text_primary };
        let fg = if enabled {
            base
        } else {
            base.gamma_multiply(DISABLED_ALPHA as f32 / 255.0)
        };
        let galley = painter.layout_no_wrap(
            text.to_owned(),
            FontId::new(TEXT_BODY, FontFamily::Proportional),
            fg,
        );
        painter.galley(
            egui::pos2(
                rect.left() + SPACE_13,
                rect.center().y - galley.size().y / 2.0,
            ),
            galley,
            fg,
        );

        // 危险徽标：右对齐的小字，和左侧竖条一起构成两重提示
        if danger {
            let tag = painter.layout_no_wrap(
                "危险".to_owned(),
                FontId::new(TEXT_CAPTION, FontFamily::Proportional),
                pal.danger,
            );
            painter.galley(
                egui::pos2(
                    rect.right() - SPACE_8 - tag.size().x,
                    rect.center().y - tag.size().y / 2.0,
                ),
                tag,
                pal.danger,
            );
        }
    }

    response
}

// ---------------- 徽标 ----------------

/// 画一个语义徽标（圆角 4、语义底色、同色文字）。
///
/// 刻意不用 `Frame` 包 `Label`：在横向布局里 Frame 的子 Ui 会拿到整行高度，
/// 徽标会被撑成一根竖条（实测顶栏里那个「机型待识别」就是被撑到了 55px 高）。
/// 这里改成自己量文字、自己分配固定高度，尺寸完全可控。
pub fn badge(ui: &mut egui::Ui, pal: &Palette, text: &str, tone: Tone) -> egui::Response {
    let font = FontId::new(TEXT_CAPTION, FontFamily::Proportional);
    let galley = ui.painter().layout_no_wrap(text.to_owned(), font, tone.fg(pal));
    let padding = egui::vec2(SPACE_8, SPACE_2);
    let size = egui::vec2(
        galley.size().x + padding.x * 2.0,
        LINE_HEIGHT,
    );
    let (rect, response) = ui.allocate_exact_size(size, egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.painter()
            .rect_filled(rect, CornerRadius::same(RADIUS_CHIP), tone.bg(pal));
        let text_pos = egui::pos2(
            rect.left() + padding.x,
            rect.center().y - galley.size().y / 2.0,
        );
        ui.painter().galley(text_pos, galley, tone.fg(pal));
    }
    response
}

/// 可折叠区块的标题行：自绘三角 + 名称 + 可选计数。
///
/// 不用 `CollapsingHeader` 的默认三角：它是个方框字形，
/// 在暗色界面里像没渲染出来的豆腐块。
/// 返回 (响应, 是否展开)。
pub fn collapsible_header(
    ui: &mut egui::Ui,
    pal: &Palette,
    id: egui::Id,
    title: &str,
    count: Option<usize>,
    default_open: bool,
) -> (egui::Response, bool) {
    let state =
        egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, default_open);
    let open = state.is_open();

    let row_h = CONTROL_HEIGHT;
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), row_h),
        egui::Sense::click(),
    );

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if response.hovered() {
            painter.rect_filled(rect, CornerRadius::same(RADIUS_CHIP), pal.bg_raised);
        }
        // 三角：向右 = 收起，向下 = 展开
        let c = egui::pos2(rect.left() + SPACE_8, rect.center().y);
        let r = SPACE_4;
        let points = if open {
            vec![
                egui::pos2(c.x - r, c.y - r * 0.5),
                egui::pos2(c.x + r, c.y - r * 0.5),
                egui::pos2(c.x, c.y + r * 0.8),
            ]
        } else {
            vec![
                egui::pos2(c.x - r * 0.5, c.y - r),
                egui::pos2(c.x - r * 0.5, c.y + r),
                egui::pos2(c.x + r * 0.8, c.y),
            ]
        };
        painter.add(egui::Shape::convex_polygon(
            points,
            pal.text_muted,
            Stroke::NONE,
        ));

        let galley = painter.layout_no_wrap(
            title.to_owned(),
            FontId::new(TEXT_BODY, FontFamily::Proportional),
            pal.text_primary,
        );
        painter.galley(
            egui::pos2(rect.left() + SPACE_21, rect.center().y - galley.size().y / 2.0),
            galley,
            pal.text_primary,
        );

        if let Some(n) = count {
            let num = painter.layout_no_wrap(
                n.to_string(),
                FontId::new(TEXT_CAPTION, FontFamily::Proportional),
                pal.text_muted,
            );
            painter.galley(
                egui::pos2(
                    rect.right() - SPACE_8 - num.size().x,
                    rect.center().y - num.size().y / 2.0,
                ),
                num,
                pal.text_muted,
            );
        }
    }

    if response.clicked() {
        let mut st = state;
        st.toggle(ui);
        st.store(ui.ctx());
    }

    (response, open)
}

/// 状态灯：一个实心圆点 + 文字。
///
/// 圆点带一圈同色低透明度外环，模拟「柔和发光」，
/// 而不是用阴影 —— 暗色界面上阴影几乎看不见。
pub fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(SPACE_8, SPACE_8), egui::Sense::hover());
    let painter = ui.painter();
    let center = rect.center();
    painter.circle_filled(center, SPACE_8 / 2.0, color.gamma_multiply(0.25));
    painter.circle_filled(center, SPACE_4 / 2.0, color);
}

/// 1px 弱分隔线（替代满屏 `ui.separator()`）。
pub fn hairline(ui: &mut egui::Ui, pal: &Palette) {
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(egui::vec2(width, 1.0), egui::Sense::hover());
    ui.painter()
        .hline(rect.x_range(), rect.center().y, Stroke::new(1.0, pal.border_weak));
}

// ---------------- 带标签的输入框 ----------------

/// 输入框上方的标签高度：12px 字 + 一点呼吸空间。
/// 单独定一个令牌而不是随手写，是因为顶栏总高度要按它推导。
pub const LABEL_H: f32 = SPACE_21;

/// 单行输入框的统一构造：**文字在框内水平、垂直都居中**。
///
/// ## 为什么必须显式写这两行
///
/// egui 的 `TextEdit` 默认是 `Align2::LEFT_TOP` —— 文字贴着左上角画。
/// 顶栏这几个框高 34、宽 55~110，而 13px 的字只有约 15px 高，
/// 于是文字上方空 11px、右边空一大截，看着像「字浮在框的左上角」。
/// 截图实测：靠左 8.5~40px、靠上 11px。
///
/// - `horizontal_align(Center)` 让文字在框宽里水平居中
/// - `vertical_align(Center)` 让文字在框高里垂直居中
///
/// 两个都必须给：只给一个的话，另一半仍然贴边，比全不居中更难看。
/// hint_text 会自动跟随同一套对齐（见 egui 的 atom_align 逻辑），
/// 所以「每次重填」这类占位文字也是居中的，不需要单独处理。
///
/// `width` 传输入框自身宽度，`.desired_width` 与它一致，
/// 保证自动换行宽度和实际框宽不会打架。
pub fn centered_edit<'a>(value: &'a mut String, width: f32, hint: Option<&str>) -> egui::TextEdit<'a> {
    let mut edit = egui::TextEdit::singleline(value)
        .horizontal_align(egui::Align::Center)
        .vertical_align(egui::Align::Center)
        .desired_width(width);
    if let Some(h) = hint {
        edit = edit.hint_text(h);
    }
    edit
}

/// 「上方常驻标签 + 输入框」的字段组。
///
/// 为什么不用 placeholder 顶替标签：placeholder 一输入就消失，
/// 用户填到一半就不知道这格是什么；出错时也看不出该改哪一格。
/// 这是 NN/g、GOV.UK、W3C 三方一致的结论，不是口味问题。
///
/// 为什么标签在上而不是在左：标签在左要额外占横向宽度，
/// 顶栏里四个字段并排时会直接把窗口挤爆；在上只吃纵向，
/// 而纵向本来就是空的（顶栏从 55 提到 89 就为了这个）。
///
/// 返回输入框的 `Response`，调用方可以接着 `.on_hover_text()` 等。
pub fn labeled_input(
    ui: &mut egui::Ui,
    pal: &Palette,
    label: &str,
    value: &mut String,
    width: f32,
) -> egui::Response {
    ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = SPACE_2;
        let label_resp = label_above(ui, pal, label, width);
        let input = ui.add_sized(
            egui::vec2(width, CONTROL_HEIGHT),
            centered_edit(value, width, None),
        );
        // egui 没有 HTML 那种声明式 label-for，
        // 只能用这个命令式关联把标签读给读屏软件。
        input.labelled_by(label_resp.id)
    })
    .inner
}

/// 画一行「精确居中于输入框正上方」的标签。
///
/// ## 为什么抽出来，且 width 必须是输入框自身的宽度
///
/// add_sized 内部用的是 Layout::centered_and_justified，
/// 标签文字是在给定宽度里**居中**绘制的。
/// 所以宽度传多少，文字就居中到多少 —— 传宽了文字就整体右移。
///
/// v1.1 首版就踩在这里：密码框把标签宽度传成了
/// width + SPACE_4 + ICON_BUTTON_W（输入框 + 间隙 + 「显示」按钮），
/// 结果「Telnet 密码」这个标签右移了半个按钮的宽度，
/// 第二个字的视觉重心压到输入框右边缘，四个标签看着参差不齐。
///
/// 现在两个调用点都只传输入框宽度，标签自然各自居中在本字段正上方。
fn label_above(ui: &mut egui::Ui, pal: &Palette, text: &str, width: f32) -> egui::Response {
    ui.add_sized(
        egui::vec2(width, LABEL_H),
        egui::Label::new(
            egui::RichText::new(text)
                .size(TEXT_CAPTION)
                .color(pal.text_secondary),
        )
        .selectable(false),
    )
}

/// 密码框：输入框 + 紧贴右侧的「显示/隐藏」按钮（+ 可选的常用口令菜单）。
///
/// 按钮和输入框同高、间距取最小刻度，视觉上是一整个控件。
///
/// `presets` 为 `Some` 时在「显示」之后再加一个「常用」菜单：
/// 点开是公开的出厂默认口令，选一条就填进去。**只填、不连、不试**。
/// 传 `None` 时行为与以前完全一致（测试里就用 `None`）。
///
/// 返回值除了输入框的 `Response`，还带上「这一帧用户选了哪条常用口令」。
/// 账号框不在本函数手里，所以账号要不要一起填由调用方决定 ——
/// 这里只负责把口令写进 `value` 并把选择报出去。
pub fn password_input(
    ui: &mut egui::Ui,
    pal: &Palette,
    label: &str,
    value: &mut String,
    show: &mut bool,
    width: f32,
    presets: Option<&'static [zte_g7615_panel::passwords::PasswordPreset]>,
) -> (egui::Response, Option<&'static zte_g7615_panel::passwords::PasswordPreset>) {
    let mut picked: Option<&'static zte_g7615_panel::passwords::PasswordPreset> = None;
    let response = ui.vertical(|ui| {
        ui.spacing_mut().item_spacing.y = SPACE_2;
        // 标签只覆盖**输入框**，不含右边的按钮：
        // 宽度给多了标签会被居中到按钮那边去（见 label_above 的说明）。
        label_above(ui, pal, label, width);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = SPACE_4;
            let input = ui.add_sized(
                egui::vec2(width, CONTROL_HEIGHT),
                centered_edit(value, width, Some("每次重填")).password(!*show),
            );
            // 图标按钮对读屏是哑的，必须给可访问名称 + 悬停说明
            let btn = ui
                .add_sized(
                    egui::vec2(ICON_BUTTON_W, CONTROL_HEIGHT),
                    ghost_button(if *show { "隐藏" } else { "显示" }, pal),
                )
                .on_hover_text("显示/隐藏密码")
                .on_hover_cursor(egui::CursorIcon::PointingHand);
            if btn.clicked() {
                *show = !*show;
                // 点完按钮把焦点还给输入框，否则用户要继续打字还得再点一次
                input.request_focus();
            }

            // ---- 常用口令菜单 ----
            // 只填输入框，不发命令、不自动连接。
            // 账号的处理交给调用方：这里只回报「选了哪一条」，
            // 因为本函数拿不到账号框，也不该知道账号框在哪。
            if let Some(list) = presets {
                let text = egui::RichText::new("常用")
                    .size(TEXT_BODY)
                    .color(pal.text_secondary);
                let mut chosen: Option<&'static zte_g7615_panel::passwords::PasswordPreset> = None;
                ui.menu_button(text, |ui| {
                    for p in list {
                        if ui
                            .button(egui::RichText::new(p.label()).size(TEXT_BODY))
                            .on_hover_text(p.tooltip())
                            .clicked()
                        {
                            chosen = Some(p);
                            ui.close();
                        }
                    }
                    ui.add_space(SPACE_4);
                    ui.label(caption(
                        "公开的出厂默认口令。能登上说明设备还是默认密码，建议改掉。",
                        pal,
                    ));
                });
                if let Some(p) = chosen {
                    *value = p.password.to_string();
                    picked = Some(p);
                    // 填完把焦点送回输入框：用户多半还要确认或改一改
                    input.request_focus();
                }
            }

            ui.allocate_space(egui::Vec2::ZERO);
            input
        })
        .inner
    })
    .inner;
    (response, picked)
}

// ---------------- 对比度（验收用） ----------------

/// sRGB 通道线性化（WCAG 2.1）。
fn linearize(c: u8) -> f32 {
    let c = c as f32 / 255.0;
    if c <= 0.039_28 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// 相对亮度（WCAG 2.1）。
pub fn relative_luminance(c: Color32) -> f32 {
    0.2126 * linearize(c.r()) + 0.7152 * linearize(c.g()) + 0.0722 * linearize(c.b())
}

/// 对比度比值。返回 ≥ 1 的数，1 表示完全相同。
pub fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let la = relative_luminance(a);
    let lb = relative_luminance(b);
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 间距刻度必须是斐波那契数列：从 8 起每项 = 前两项之和。
    ///
    /// 前两项 2、4 是「起点」：真实的斐波那契从 1、2 开始，
    /// 取偶数档是为了让最小间距在 100% 缩放下不糊成一条线。
    /// 所以从第 3 项（8）之后才做严格求和断言。
    /// 这条测试是「固定间隔」这个设计要求的机器可验证版本。
    #[test]
    fn 间距刻度是斐波那契数列() {
        let scale = [
            SPACE_2, SPACE_4, SPACE_8, SPACE_13, SPACE_21, SPACE_34, SPACE_55, SPACE_89,
        ];
        // 8 + 13 = 21 起严格成立
        for i in 4..scale.len() {
            assert!(
                (scale[i - 1] + scale[i - 2] - scale[i]).abs() < 0.5,
                "第 {i} 项 {} 不等于前两项之和 {}",
                scale[i],
                scale[i - 1] + scale[i - 2]
            );
        }
        // 8 与 13 也必须能由 2、4 推出来（4 + 2 = 6，向上取到 8）
        assert!((SPACE_8 - (SPACE_4 + SPACE_2)).abs() <= 2.0);
        // 相邻比值必须收敛到 φ（1.618）
        for i in 3..scale.len() {
            let ratio = scale[i] / scale[i - 1];
            assert!((ratio - 1.618).abs() < 0.02, "第 {i} 项比值 {ratio} 偏离 φ");
        }
    }

    /// 字号必须严格按 φ 逐级放大（允许取整误差）。
    #[test]
    fn 字号按黄金比例分级() {
        let phi = 1.618_f32;
        assert!((TEXT_BODY / TEXT_CAPTION - 1.083).abs() < 0.1); // 12→13 是基准档
        assert!((TEXT_SECTION - TEXT_BODY * phi).abs() <= 0.5);
        assert!((TEXT_DISPLAY - TEXT_SECTION * phi).abs() <= 0.5);
        assert!((LINE_HEIGHT - TEXT_BODY * phi).abs() <= 0.5);
    }

    /// 窗口与侧栏的尺寸关系必须能由 φ 推出来。
    #[test]
    fn 窗口与侧栏尺寸由黄金比例推导() {
        let phi = 1.618_f32;
        assert!((WINDOW_DEFAULT_W / phi - WINDOW_DEFAULT_H).abs() < 2.0);
        assert!((WINDOW_MIN_W / phi - WINDOW_MIN_H).abs() < 2.0);
        assert!((WINDOW_DEFAULT_W / phi.powi(3) - SIDEBAR_DEFAULT).abs() < 2.0);
        assert!((SIDEBAR_DEFAULT / phi - SIDEBAR_MIN).abs() < 2.0);
        assert!((SIDEBAR_DEFAULT * phi - SIDEBAR_MAX).abs() < 2.0);
    }

    /// 顶栏高度必须真的装得下两行内容。
    ///
    /// 这条测试是 v1.1 首版真实事故的护栏：
    /// 当时 TOPBAR_HEIGHT 取 55（单行时代的值），两行内容放不下，
    /// 加上一个会撑高的错误布局，顶栏被撑到 486px，
    /// 四个输入框全被挤出可视区 —— 用户看到的就是「一片空白，启动失败」。
    #[test]
    fn 顶栏高度装得下两行内容() {
        // 框架上下内边距 + 第 1 行 + 行间距 + 第 2 行（标签 + 间距 + 输入框）
        let need = SPACE_8 * 2.0            // inner_margin 上下
            + SPACE_21                      // 第 1 行：品牌/机型/状态
            + SPACE_8                       // 两行之间的间距
            + LABEL_H + SPACE_2 + CONTROL_HEIGHT; // 第 2 行：标签 + 间距 + 输入框
        assert!(
            TOPBAR_HEIGHT >= need - 0.5,
            "顶栏 {TOPBAR_HEIGHT} 装不下两行内容，至少需要 {need}"
        );
        // 也不能高得离谱：顶栏超过窗口 1/5 就是在浪费屏幕
        assert!(
            TOPBAR_HEIGHT <= WINDOW_MIN_H / 5.0,
            "顶栏 {TOPBAR_HEIGHT} 超过最小窗口高度的 1/5（{}）",
            WINDOW_MIN_H / 5.0
        );
    }

    /// 无障碍硬指标：正文 ≥ 4.5:1，次要 ≥ 4.5:1，弱文字 ≥ 3:1。
    /// 背景取最不利的三种表面。**两套配色都必须满足**。
    #[test]
    fn 两套配色的文字对比度都满足无障碍要求() {
        for (name, p) in [("暗色", &DARK), ("浅色", &LIGHT)] {
            for bg in [p.bg_base, p.bg_panel, p.bg_raised] {
                assert!(
                    contrast_ratio(p.text_primary, bg) >= 4.5,
                    "{name} 主文字对 {bg:?} 只有 {}",
                    contrast_ratio(p.text_primary, bg)
                );
                assert!(
                    contrast_ratio(p.text_secondary, bg) >= 4.5,
                    "{name} 次文字对 {bg:?} 只有 {}",
                    contrast_ratio(p.text_secondary, bg)
                );
                assert!(
                    contrast_ratio(p.text_muted, bg) >= 3.0,
                    "{name} 弱文字对 {bg:?} 只有 {}",
                    contrast_ratio(p.text_muted, bg)
                );
            }
        }
    }

    /// 语义色必须能在各自底色上读清（危险/成功/警告都是要命的提示）。
    #[test]
    fn 两套配色的语义色都可读() {
        for (name, p) in [("暗色", &DARK), ("浅色", &LIGHT)] {
            for c in [p.success, p.danger, p.warning, p.accent, p.info] {
                assert!(
                    contrast_ratio(c, p.bg_base) >= 4.5,
                    "{name} 语义色 {c:?} 对 bg_base 只有 {}",
                    contrast_ratio(c, p.bg_base)
                );
            }
        }
    }

    /// 浅色主题的验收硬指标（验收清单第 4 条）：
    /// 七个关键令牌对 `#FFFFFF` 与 `#F6F8FA` 都必须 ≥ 4.5:1。
    /// 这条测试是「文档里写的对比度不是编的」的机器证明。
    #[test]
    fn 浅色关键令牌对白底和浅灰底都达标() {
        let white = Color32::from_rgb(0xFF, 0xFF, 0xFF);
        let light_gray = Color32::from_rgb(0xF6, 0xF8, 0xFA);
        let keys = [
            ("TEXT_PRIMARY", LIGHT.text_primary),
            ("TEXT_SECONDARY", LIGHT.text_secondary),
            ("TEXT_MUTED", LIGHT.text_muted),
            ("ACCENT", LIGHT.accent),
            ("SUCCESS", LIGHT.success),
            ("DANGER", LIGHT.danger),
            ("WARNING", LIGHT.warning),
        ];
        for (name, c) in keys {
            for (bg_name, bg) in [("白底", white), ("浅灰底", light_gray)] {
                let r = contrast_ratio(c, bg);
                assert!(r >= 4.5, "浅色 {name} 对{bg_name} 只有 {r:.2}:1");
            }
        }
    }

    /// 暗色令牌**一个字节都不许动**（验收清单第 3 条）。
    ///
    /// 这些值是 v1.0 发布时的实测基准，重构成 Palette 之后必须原样保留。
    /// 写死十六进制而不是引用 DARK 自身，否则改错了测试也跟着变，等于没测。
    #[test]
    fn 暗色令牌与v1_0逐值一致() {
        let expect: [(&str, u32, Color32); 18] = [
            ("BG_BASE", 0x0E1116, DARK.bg_base),
            ("BG_PANEL", 0x151A21, DARK.bg_panel),
            ("BG_RAISED", 0x1C232C, DARK.bg_raised),
            ("BORDER_WEAK", 0x2A333F, DARK.border_weak),
            ("BORDER_STRONG", 0x3A4655, DARK.border_strong),
            ("TEXT_PRIMARY", 0xE6EDF3, DARK.text_primary),
            ("TEXT_SECONDARY", 0x9BA9B8, DARK.text_secondary),
            ("TEXT_MUTED", 0x6B7A8C, DARK.text_muted),
            ("ACCENT", 0x4CC2FF, DARK.accent),
            ("ACCENT_HOVER", 0x6FD0FF, DARK.accent_hover),
            ("SUCCESS", 0x3FB950, DARK.success),
            ("DANGER", 0xF85149, DARK.danger),
            ("WARNING", 0xD29922, DARK.warning),
            ("INFO", 0x58A6FF, DARK.info),
            ("DANGER_BG", 0x2D1417, DARK.danger_bg),
            ("SUCCESS_BG", 0x0F2417, DARK.success_bg),
            ("WARNING_BG", 0x2A2113, DARK.warning_bg),
            ("INFO_BG", 0x12233A, DARK.info_bg),
        ];
        for (name, hex, actual) in expect {
            let want = Color32::from_rgb(
                ((hex >> 16) & 0xFF) as u8,
                ((hex >> 8) & 0xFF) as u8,
                (hex & 0xFF) as u8,
            );
            assert_eq!(actual, want, "暗色 {name} 被改动了");
        }
    }

    /// `palette(dark_mode)` 必须真的返回两套不同的配色 ——
    /// 防止以后有人把它写成「永远返回 DARK」而没人发现。
    #[test]
    fn palette按主题返回不同配色() {
        assert_eq!(palette(true), &DARK);
        assert_eq!(palette(false), &LIGHT);
        assert_ne!(DARK.bg_base, LIGHT.bg_base);
        assert_ne!(DARK.text_primary, LIGHT.text_primary);
    }

    /// 「跟随系统」必须真的跟着系统走，而不是悄悄钉死在浅色。
    ///
    /// 默认值改成 System 之后，最大的风险是「System 其实等于 Light」——
    /// 界面看着正常，但用户把系统切成深色时界面纹丝不动。
    /// 这里给 egui 喂两个不同的 system_theme，断言主题真的跟着变。
    #[test]
    fn 跟随系统会跟随系统主题() {
        for (system, want_dark) in [(egui::Theme::Dark, true), (egui::Theme::Light, false)] {
            let ctx = egui::Context::default();
            install(&ctx, ThemePreference::System);

            // 一定要在同一帧里读 dark_mode：
            // 再开一帧会碰上「上一帧的 FullOutput 还没释放」的断言。
            let mut dark = None;
            let output = ctx.run_ui(
                egui::RawInput {
                    system_theme: Some(system),
                    ..Default::default()
                },
                |ui| {
                    dark = Some(ui.visuals().dark_mode);
                    ui.label("x");
                },
            );
            output.drop_without_applying_deltas();

            assert_eq!(
                dark,
                Some(want_dark),
                "system_theme={system:?} 时界面没有跟着变"
            );
        }
    }

    /// 四个标签必须各自**居中**在自己输入框的正上方。
    ///
    /// 这条测试是 v1.1 首版真实缺陷的护栏：
    /// 密码框把标签宽度传成了「输入框 + 间隙 + 显示按钮」，
    /// 于是「Telnet 密码」整体右移了半个按钮的宽度，
    /// 截图实测偏了 42 像素，四个标签看着参差不齐。
    ///
    /// 这里真的跑一遍 egui 布局，再从绘制指令里读文字的实际位置，
    /// 而不是只断言「宽度参数传对了」—— 后者改不动实现也照样能过。
    #[test]
    fn 每个标签都居中在自己输入框正上方() {
        let ctx = egui::Context::default();
        let pal = &LIGHT;

        let mut host = String::from("192.168.1.1");
        let mut port = String::from("23");
        let mut user = String::from("root");
        let mut pwd = String::new();
        let mut show_pwd = false;
        // (标签文字, 输入框矩形)
        let mut fields: Vec<(&'static str, egui::Rect)> = Vec::new();

        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1400.0, 240.0),
                )),
                ..Default::default()
            },
            |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = SPACE_13;
                    fields.push((
                        "光猫 IP",
                        labeled_input(ui, pal, "光猫 IP", &mut host, FIELD_W).rect,
                    ));
                    fields.push((
                        "端口",
                        labeled_input(ui, pal, "端口", &mut port, FIELD_W_PORT).rect,
                    ));
                    fields.push((
                        "Telnet 账号",
                        labeled_input(ui, pal, "Telnet 账号", &mut user, FIELD_W).rect,
                    ));
                    fields.push((
                        "Telnet 密码",
                        password_input(ui, pal, "Telnet 密码", &mut pwd, &mut show_pwd, FIELD_W_PWD, None)
                            .0
                            .rect,
                    ));
                });
            },
        );

        // 从绘制指令里取每个标签文字的真实横向中心
        let mut centers: Vec<(&'static str, f32)> = Vec::new();
        for clipped in &output.shapes {
            if let egui::Shape::Text(t) = &clipped.shape {
                let text = t.galley.job.text.as_str();
                // 只认那四个标签本身；输入框自身也会画文字，按内容排除掉
                if let Some((label, _)) = fields.iter().find(|(l, _)| *l == text) {
                    centers.push((label, t.pos.x + t.galley.rect.center().x));
                }
            }
        }
        // egui 要求把这一帧的纹理增量显式丢弃，否则 Drop 时会 panic。
        output.drop_without_applying_deltas();

        assert_eq!(centers.len(), 4, "应找到 4 个标签文字，实际 {centers:?}");
        for (label, field_rect) in &fields {
            let center = centers
                .iter()
                .find(|(l, _)| *l == *label)
                .map(|(_, c)| *c)
                .unwrap_or_else(|| panic!("没找到标签「{label}」的绘制指令"));
            let want = field_rect.center().x;
            assert!(
                (center - want).abs() <= 1.0,
                "标签「{label}」中心在 {center:.1}，输入框中心在 {want:.1}，偏了 {:.1}px",
                center - want
            );
        }
    }

    /// 加了「常用」菜单之后，密码框的标签仍然必须居中在自己输入框正上方。
    ///
    /// 菜单按钮在输入框**右边**，如果实现里把它算进了标签宽度
    /// （`label_above` 的 width 参数），标签就会被整体推向右边 ——
    /// 这正是 v1.1 首版踩过的坑（「显示」按钮那一版偏了 42px）。
    #[test]
    fn 带常用菜单时密码标签仍居中() {
        let ctx = egui::Context::default();
        let pal = &LIGHT;

        let mut pwd = String::new();
        let mut show_pwd = false;
        let mut field: Option<egui::Rect> = None;

        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1400.0, 240.0),
                )),
                ..Default::default()
            },
            |ui| {
                let (resp, _) = password_input(
                    ui,
                    pal,
                    "Telnet 密码",
                    &mut pwd,
                    &mut show_pwd,
                    FIELD_W_PWD,
                    Some(zte_g7615_panel::passwords::PRESETS),
                );
                field = Some(resp.rect);
            },
        );

        let mut center: Option<f32> = None;
        let mut menu_seen = false;
        for clipped in &output.shapes {
            if let egui::Shape::Text(t) = &clipped.shape {
                let text = t.galley.job.text.as_str();
                if text == "Telnet 密码" {
                    center = Some(t.pos.x + t.galley.rect.center().x);
                }
                if text == "常用" {
                    menu_seen = true;
                }
            }
        }
        output.drop_without_applying_deltas();

        assert!(menu_seen, "应画出「常用」菜单按钮");
        let field = field.expect("应拿到输入框矩形");
        let center = center.expect("应找到「Telnet 密码」标签");
        let want = field.center().x;
        assert!(
            (center - want).abs() <= 1.0,
            "带菜单时标签中心 {center:.1}，输入框中心 {want:.1}，偏了 {:.1}px",
            center - want
        );
    }

    /// 最小窗口宽度下，顶栏第二行必须放得下四个字段 + 「常用」菜单 + 连接按钮。
    ///
    /// 这条守的是「以后再加一个控件把顶栏挤爆」：
    /// 挤爆的表现不是报错，而是连接按钮被推出可视区 —— 用户看到的就是连不上。
    #[test]
    fn 最小窗口宽度下顶栏放得下() {
        let ctx = egui::Context::default();
        let pal = &LIGHT;

        let mut host = String::from("192.168.1.1");
        let mut port = String::from("23");
        let mut user = String::from("root");
        let mut pwd = String::new();
        let mut show_pwd = false;
        let mut rects: Vec<egui::Rect> = Vec::new();

        // 顶栏内容宽度 = 最小窗口宽 - 左右内边距
        let content_w = WINDOW_MIN_W - SPACE_21 * 2.0;
        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(content_w, 240.0),
                )),
                ..Default::default()
            },
            |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = SPACE_13;
                    rects.push(labeled_input(ui, pal, "光猫 IP", &mut host, FIELD_W).rect);
                    rects.push(labeled_input(ui, pal, "端口", &mut port, FIELD_W_PORT).rect);
                    rects.push(labeled_input(ui, pal, "Telnet 账号", &mut user, FIELD_W).rect);
                    let (resp, _) = password_input(
                        ui,
                        pal,
                        "Telnet 密码",
                        &mut pwd,
                        &mut show_pwd,
                        FIELD_W_PWD,
                        Some(zte_g7615_panel::passwords::PRESETS),
                    );
                    rects.push(resp.rect);
                });
            },
        );
        output.drop_without_applying_deltas();

        assert_eq!(rects.len(), 4);
        let used = rects.last().unwrap().right() - rects[0].left();
        let need = used + SPACE_89; // 连接按钮宽度
        assert!(
            need <= content_w,
            "顶栏内容宽 {need:.0}px 超过可用 {content_w:.0}px，连接按钮会被挤出可视区"
        );
    }

    /// 输入框里的文字也必须**水平 + 垂直居中**，不能贴左上角。
    ///
    /// egui 的 `TextEdit` 默认是 `Align2::LEFT_TOP`，不显式设的话
    /// 文字会贴在框的左上角：截图实测靠左 8.5~40px、靠上 11px，
    /// 34px 高的框里字挤在顶部，看着像框没对齐。
    ///
    /// 这里从绘制指令里读输入文字的位置，和输入框矩形比中心。
    #[test]
    fn 输入框里的文字居中() {
        let ctx = egui::Context::default();
        let pal = &LIGHT;

        let mut ip = String::from("192.168.1.1");
        let mut port = String::from("23");
        let mut user = String::from("root");
        let mut pwd = String::new();
        let mut show_pwd = false;
        let mut rects: Vec<(&'static str, egui::Rect)> = Vec::new();

        let output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1400.0, 240.0),
                )),
                ..Default::default()
            },
            |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = SPACE_13;
                    rects.push(("192.168.1.1", labeled_input(ui, pal, "a", &mut ip, FIELD_W).rect));
                    rects.push(("23", labeled_input(ui, pal, "b", &mut port, FIELD_W_PORT).rect));
                    rects.push(("root", labeled_input(ui, pal, "c", &mut user, FIELD_W).rect));
                    rects.push((
                        "每次重填",
                        password_input(ui, pal, "d", &mut pwd, &mut show_pwd, FIELD_W_PWD, None)
                            .0
                            .rect,
                    ));
                });
            },
        );

        // 密码框是空的，画的是 hint_text，所以按 hint 文字比对
        let mut found: Vec<(&str, egui::Rect, egui::Rect)> = Vec::new();
        for clipped in &output.shapes {
            if let egui::Shape::Text(t) = &clipped.shape {
                let text = t.galley.job.text.as_str();
                if let Some((want, field)) = rects.iter().find(|(w, _)| *w == text) {
                    // galley 的实际墨迹矩形 = rect 按 pos 平移
                    let ink = t.galley.rect.translate(t.pos.to_vec2());
                    found.push((want, *field, ink));
                }
            }
        }
        output.drop_without_applying_deltas();

        assert_eq!(found.len(), 4, "应找到 4 段输入文字，实际 {}", found.len());
        for (text, field, ink) in found {
            let dx = ink.center().x - field.center().x;
            let dy = ink.center().y - field.center().y;
            assert!(
                dx.abs() <= 2.0,
                "「{text}」水平没居中：文字 {:.1} vs 输入框 {:.1}，差 {dx:.1}px",
                ink.center().x,
                field.center().x
            );
            assert!(
                dy.abs() <= 2.0,
                "「{text}」垂直没居中：文字 {:.1} vs 输入框 {:.1}，差 {dy:.1}px",
                ink.center().y,
                field.center().y
            );
        }
    }
}
