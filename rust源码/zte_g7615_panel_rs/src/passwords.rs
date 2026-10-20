//! 常用 telnet 默认口令清单。
//!
//! ## 这是什么、不是什么
//!
//! 这是一份**公开可检索的出厂默认口令**，只用来省掉「我记不住这台机器默认密码
//! 是什么」的麻烦。点一下填进输入框，仅此而已：
//!
//! - 不自动尝试、不批量试错、不爆破
//! - 不落盘、不入日志、不进截图（见文件末尾的测试）
//! - 不改账号框里用户已经填好的内容
//!
//! ## 与「密码不落盘」红线的关系
//!
//! 项目红线是「**你自己改过的密码**不落盘」。这里列的是厂商出厂值，
//! 本来就印在别人的论坛帖和开源工具 README 里，写进源码不泄露任何东西。
//! 两件事不能混为一谈：把 `Zte521` 写进源码没有风险，
//! 把用户自己设的密码写进源码才是事故。
//!
//! ## 每条都要有出处
//!
//! 清单里的每一条都注明了来源与可信度。没有来源的（论坛里口口相传的
//! `admin/admin` 之类）一律不收 —— 收进来就是拿用户的设备去试。

/// 一条默认口令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PasswordPreset {
    /// 账号。选中时可以顺带填进账号框（仅当账号框为空）。
    pub user: &'static str,
    /// 口令。
    pub password: &'static str,
    /// 菜单里显示的归属说明。
    pub vendor: &'static str,
}

impl PasswordPreset {
    /// 菜单项文字，例如 `root / root · F650 默认`。
    pub fn label(&self) -> String {
        format!("{} / {} · {}", self.user, self.password, self.vendor)
    }

    /// 悬停说明：这条是从哪来的、什么时候适用。
    pub fn tooltip(&self) -> String {
        format!(
            "账号 {}，口令 {}\n{}\n公开的出厂默认值，不是本机保存的密码。",
            self.user, self.password, self.vendor
        )
    }
}

/// 常用默认口令。**顺序即菜单顺序**：最可能用到的排前面。
///
/// 排序依据：本程序面向中兴光猫，先中兴自己的两组，再运营商超管。
pub const PRESETS: &[PasswordPreset] = &[
    // F650 天翼网关 3.0 换掉 telnetd 之后，认证改读 OpenWrt 子系统的
    // /opt/upt/framework/saf/rootfs/etc/shadow，账号与口令都是 root。
    // 两条独立来源（宽带技术网开 telnet 教程、恩山免拆机教程）互相印证。
    PasswordPreset {
        user: "root",
        password: "root",
        vendor: "F650 默认（换 telnetd 后）",
    },
    // 中兴固件自带的 telnet 口令。「固化 Telnet」写进 TelnetCfg 的就是这组，
    // 两个独立的 zteOnu 仓库 README 都写明 --telnet 用的 user: root / pass: Zte521。
    PasswordPreset {
        user: "root",
        password: "Zte521",
        vendor: "中兴 telnet 固件默认",
    },
    // 电信超管 / factory 模式口令。zteOnu 的默认认证参数与多篇汇总一致。
    // 主要用在 Web 8080 与 factory 接口；telnet 是否认它取决于固件。
    PasswordPreset {
        user: "telecomadmin",
        password: "nE7jA%5m",
        vendor: "电信超管 / factory",
    },
    // 联通超管。主要用在 Web 管理页 http://192.168.1.1/cu.html。
    PasswordPreset {
        user: "CUAdmin",
        password: "CUAdmin",
        vendor: "联通超管",
    },
];

/// 选中一条预设之后的填充结果。
///
/// 规则只有两条，但都很重要：
///
/// 1. **口令总是覆盖** —— 用户点它就是为了换口令。
/// 2. **账号只在为空时填** —— 用户自己敲的账号优先。
///    否则点一下 `telecomadmin` 就把辛苦敲的 `root` 冲掉了，
///    而界面上看不出「它替我改了账号」。
///
/// 不做「顺便帮你连一下」：连不连是用户的决定，
/// 自动连接会把「填错口令被设备拒绝」变成一次莫名其妙的失败弹窗。
pub fn apply(preset: &PasswordPreset, user: &str, password: &str) -> (String, String) {
    let new_user = if user.trim().is_empty() {
        preset.user.to_string()
    } else {
        user.to_string()
    };
    let _ = password; // 口令永远被覆盖，旧值不参与判断
    (new_user, preset.password.to_string())
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    /// 清单本身必须干净：不能有空项、不能有重复。
    ///
    /// 重复项在菜单里就是两条一模一样的东西，用户会以为其中一条是错的。
    #[test]
    fn 预设口令清单合法() {
        assert!(!PRESETS.is_empty(), "清单不能为空");

        let mut seen: Vec<(&str, &str)> = Vec::new();
        for p in PRESETS {
            assert!(!p.user.is_empty(), "账号不能为空");
            assert!(!p.password.is_empty(), "口令不能为空");
            assert!(!p.vendor.is_empty(), "归属说明不能为空，否则菜单里看不出这条是干什么的");
            // 空白字符会让「账号 / 口令」在输入框里变成一个不可见的多余字符
            assert!(
                !p.user.chars().any(char::is_whitespace),
                "账号 {:?} 不该含空白",
                p.user
            );
            assert!(
                !p.password.chars().any(char::is_whitespace),
                "口令不该含空白（{:?}）",
                p.vendor
            );
            let key = (p.user, p.password);
            assert!(!seen.contains(&key), "重复的口令项：{key:?}");
            seen.push(key);
        }
    }

    /// F650 那一条必须在清单里，而且排在第一个。
    ///
    /// 这是用户实测过的那台机器：开 telnet 换掉 telnetd 之后账号口令都是 root。
    /// 排第一是因为本程序就是为中兴光猫做的，而这是最常见的落地状态。
    #[test]
    fn F650默认口令在清单首位() {
        let first = &PRESETS[0];
        assert_eq!(first.user, "root");
        assert_eq!(first.password, "root");
        assert!(first.vendor.contains("F650"), "首条应标明 F650");
    }

    /// 选中预设时的填充规则。
    #[test]
    fn 选中预设时的填充规则() {
        let f650 = &PRESETS[0];
        assert_eq!(f650.user, "root");

        // 账号为空 → 一并填上
        let (u, p) = apply(f650, "", "");
        assert_eq!(u, "root");
        assert_eq!(p, "root");

        // 只有空白也算空（用户可能敲了个空格）
        let (u, _) = apply(f650, "   ", "");
        assert_eq!(u, "root", "全空白应视为空");

        // 账号已有内容 → 不覆盖，但口令照换
        let (u, p) = apply(f650, "telecomadmin", "旧的");
        assert_eq!(u, "telecomadmin", "用户自己填的账号不能被冲掉");
        assert_eq!(p, "root", "口令必须被覆盖");

        // 换一条：口令随之改变，账号仍保留
        let cu = &PRESETS[3];
        assert_eq!(cu.user, "CUAdmin");
        let (u, p) = apply(cu, "root", "root");
        assert_eq!(u, "root", "账号保留");
        assert_eq!(p, "CUAdmin", "口令换成新选的");
    }

    /// 菜单文字与悬停说明都要能读。
    #[test]
    fn 菜单文字包含账号口令与归属() {
        for p in PRESETS {
            let label = p.label();
            assert!(label.contains(p.user), "菜单项缺账号：{label}");
            assert!(label.contains(p.password), "菜单项缺口令：{label}");
            assert!(label.contains(p.vendor), "菜单项缺归属：{label}");

            let tip = p.tooltip();
            assert!(tip.contains(p.user) && tip.contains(p.password));
            assert!(tip.contains("不是本机保存的密码"), "悬停要说清这不是本机存的密码");
        }
    }

    /// 预设口令**绝不能**被写进连接设置文件。
    ///
    /// 红线：`数据/连接设置.txt` 只有 address / port / user 三项。
    /// 这条测试把「清单」和「落盘」两条路径钉在一起 ——
    /// 以后谁想在设置文件里加一个 password 字段，会立刻在这里失败。
    ///
    /// ⚠ 不能简单地断言 `!text.contains(p.password)`：
    /// 默认账号恰好是 `root`，而 `root/root` 那条的口令也是 `root`，
    /// 于是 `user=root` 这一行会让断言假失败。
    /// 所以要断言的是**键的集合**，而不是「这段文字里有没有出现过这个词」。
    #[test]
    fn 预设口令不会被写进连接设置() {
        let s = crate::settings::ConnectionSettings::default();
        let text = s.render();

        // 1) 键只能是这三项，一个都不能多
        let mut keys: Vec<&str> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (k, _) = line.split_once('=').expect("每行都该是 key=value");
            keys.push(k.trim());
        }
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["address", "port", "user"],
            "连接设置只允许 address / port / user 三项，实际：{keys:?}\n{text}"
        );

        // 2) 除了「账号」这一项，任何预设口令都不该作为值出现
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            if k.trim() == "user" {
                continue; // 账号本来就该存
            }
            let v = v.trim();
            for p in PRESETS {
                assert!(
                    v != p.password,
                    "{} 的口令出现在了 {}= 里：\n{text}",
                    p.vendor,
                    k.trim()
                );
            }
        }
    }
}
