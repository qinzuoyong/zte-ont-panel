//! 构建脚本：两件事。
//!
//! ## 1. 加大主线程栈
//!
//! Windows 给主线程的默认栈只有 1MB，而 eframe/wgpu 的初始化路径
//! （图形适配器枚举 + shader 编译/校验）会把它用尽，表现为程序一启动就
//! `thread 'main' has overflowed its stack` 直接崩溃（退出码 0xC0000005）。
//!
//! 不能改成「把 GUI 放到子线程跑」：winit 在 Windows 上明确拒绝在非主线程
//! 创建事件循环（会 panic 提示要用 EventLoopBuilderExtWindows::any_thread）。
//! 所以正确做法是在链接期把主线程栈调大。
//!
//! ## 2. 写入 Windows 版本资源（VERSIONINFO）
//!
//! 未签名的 exe 如果连「我是谁」都没有，属性和 SmartScreen 里就是一片空白，
//! 只按哈希算信誉，观感与拦截率都最差。这里把产品名、公司名、文件描述、
//! 版本号、版权写进 PE 资源段，让文件自带身份信息。
//!
//! ### 为什么不用 windres
//!
//! 常规做法是写一个 `.rc` 交给 GNU 的 `windres` 编译。但本机实测走不通：
//!
//! 1. `windres` 预处理 `.rc` 时要调用 `gcc`，而本机 mingw 是精简版
//!    （mingwmini），只有 `windres` 没有 `gcc`，直接报 `preprocessing failed`。
//! 2. 用 `--preprocessor=` 换成系统自带的 `more.com` 可以绕过第 1 点，
//!    但 `more.com` 会把 LF 扩成 CRLF（实测 512 → 534 字节），文本被改坏。
//! 3. 即使传入字节精确的流，`windres` 也无法把非 ASCII 字符正确写进资源：
//!    ASCII 字符串能进（`PlainAsciiName` 可查到），中文和 Latin-1 都丢失。
//!    换 `-c` 代码页、UTF-16 BOM、UTF-8 都没用。
//!
//! 所以这里**直接手写 `.res` 资源二进制**。`.res` 是微软公开的简单结构
//! （一串「资源条目」+ 一个可选的字符串表），不需要任何外部工具，也就不受
//! 上述编码问题影响，中文能原样写进去。
//!
//! 链接方式：`cargo:rustc-link-arg-bins=<生成的目标文件>`。
//! 目标文件用 COFF 格式自己装配（`.rsrc` 段 + 段头），交给 ld 即可。

use std::path::PathBuf;

/// 主线程栈大小：16 MiB。
const STACK_BYTES: usize = 16 * 1024 * 1024;

/// 资源类型：VERSION（RT_VERSION = 16）。
const RT_VERSION: u32 = 16;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();

    // 非 Windows 平台什么都不用做
    if target_os != "windows" {
        return;
    }

    // ---- 1. 主线程栈 ----
    let stack_arg = if target_env == "msvc" {
        format!("/STACK:{STACK_BYTES}")
    } else {
        // GNU (mingw) 工具链
        format!("-Wl,--stack,{STACK_BYTES}")
    };
    println!("cargo:rustc-link-arg-bins={stack_arg}");

    // ---- 2. 版本资源 ----
    if let Some(obj) = write_resource_object() {
        println!("cargo:rustc-link-arg-bins={}", obj.display());
    }
}

/// 生成版本资源并写成 COFF 目标文件，返回其路径。
///
/// 失败返回 None（例如 OUT_DIR 拿不到），此时只是 exe 属性页为空，不影响构建。
fn write_resource_object() -> Option<PathBuf> {
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR")?);

    // ---- 取包元数据 ----
    let pkg_version = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let pkg_name = std::env::var("CARGO_PKG_NAME").unwrap_or_else(|_| "app".into());
    let pkg_desc = std::env::var("CARGO_PKG_DESCRIPTION").unwrap_or_default();
    let authors = std::env::var("CARGO_PKG_AUTHORS").unwrap_or_default();
    let license = std::env::var("CARGO_PKG_LICENSE").unwrap_or_default();

    // 文件属性页里显示的「文件说明」。产品已通用化，不再是 G7615 专用。
    let display_name = "中兴光猫控制面板";
    let company = if authors.is_empty() { "yong".to_string() } else { authors };
    let comment = if pkg_desc.is_empty() { display_name.to_string() } else { pkg_desc };
    let copyright = if license.is_empty() {
        format!("Copyright (C) 2026 {company}")
    } else {
        format!("Copyright (C) 2026 {company}. License: {license}")
    };

    // 版本号 "1.0.0" → [1,0,0,0]
    let mut ver = [0u16; 4];
    for (i, seg) in pkg_version.split('.').take(4).enumerate() {
        ver[i] = seg.trim().parse::<u16>().unwrap_or(0);
    }
    let ver_str = format!("{}.{}.{}.{}", ver[0], ver[1], ver[2], ver[3]);

    // ---- 组装 VS_VERSIONINFO 结构 ----
    //
    // 结构（全部按 32 位对齐）：
    //   VS_VERSIONINFO
    //     wLength, wValueLength(=sizeof(VS_FIXEDFILEINFO)=52), wType(=0)
    //     szKey = "VS_VERSION_INFO" (UTF-16, 双 \0 结尾)
    //     padding 到 4 字节
    //     Value = VS_FIXEDFILEINFO (52 字节)
    //     padding
    //     Children: StringFileInfo, VarFileInfo
    let fixed = build_fixed_file_info(ver);

    // 版本资源用的语言 / 代码页。
    //
    // 这三处必须**保持一致**，否则 Windows 按 Translation 里登记的键去
    // StringFileInfo 下找 StringTable 会落空，属性页显示空白：
    //   1. 资源树第 3 层的语言 ID（build_pe_resource_tree 的 res_lang）
    //   2. StringTable 的键（形如 "040904b0"）
    //   3. VarFileInfo\Translation 里的 (语言, 代码页)
    // 统一用 0x0409 + 0x04B0（en-US + Unicode），这与 windres 的默认值一致。
    const RES_LANG: u16 = 0x0804;
    const RES_CODEPAGE: u16 = 0x04B0;
    /// StringTable 键里用的语言部分。与资源树第 3 层的 LangID 解耦：
    /// 资源树用 0x0409（Windows 查找版本资源的默认回退语言），
    /// StringTable 键用 0x0804，二者不一致时 Windows 仍能按 Translation 命中。
    const TABLE_LANG: u16 = 0x0804;
    let table_key = format!("{TABLE_LANG:04x}{RES_CODEPAGE:04x}");

    let mut string_file_info = Vec::new();
    {
        let mut table = Vec::new();
        let entries: [(&str, &str); 9] = [
            ("CompanyName", &company),
            ("FileDescription", display_name),
            ("FileVersion", &ver_str),
            ("InternalName", &pkg_name),
            ("LegalCopyright", &copyright),
            ("OriginalFilename", &format!("{pkg_name}.exe")),
            ("ProductName", display_name),
            ("ProductVersion", &ver_str),
            ("Comments", &comment),
        ];
        for (key, val) in entries {
            table.extend(build_string_entry(key, val));
        }
        let table_block = build_node(&table_key, 0, &table);
        string_file_info = build_node("StringFileInfo", 0, &table_block);
    }

    let mut var_file_info = Vec::new();
    {
        // Translation 的值是一个 (语言, 代码页) 对，共 4 字节。
        // 对 Translation 而言 wValueLength 是**字节数**（不是字符数）。
        let mut trans = Vec::new();
        trans.extend_from_slice(&RES_LANG.to_le_bytes());
        trans.extend_from_slice(&RES_CODEPAGE.to_le_bytes());
        // 用带 Value 的节点构造器，保证 wLength 正确
        let block = build_node_with_value("Translation", trans.len() as u16, &trans, &[]);
        var_file_info = build_node("VarFileInfo", 0, &block);
    }

    // 顶层节点：Value 是 52 字节的 VS_FIXEDFILEINFO
    let mut children = Vec::new();
    children.extend_from_slice(&string_file_info);
    children.extend_from_slice(&var_file_info);
    let version_info = build_version_root(&fixed, &children);

    // 顶层要 4 字节对齐
    let mut version_info = version_info;
    while version_info.len() % 4 != 0 {
        version_info.push(0);
    }

    // 语言 ID 用 0x0409（en-US）。
    //
    // 第 3 层的语言 ID 必须是一个「真实存在」的语言，否则
    // `GetFileVersionInfoSize` 会返回 0 并报 ERROR_RESOURCE_TYPE_NOT_FOUND(1813)：
    // Windows 会按当前 UI 语言去第 3 层找匹配项，再回落到 0x0409（英文），
    // 找不到就认为版本资源不存在。实测 windres 产出的也是 0x0409。
    //
    // 注意：这里只影响「去哪一层取数据」，不影响字符串本身的编码——
    // 中文照样以 UTF-16 存进资源、照样正确显示。
    let (res, reloc_offsets) = build_pe_resource_tree(RT_VERSION, RES_LANG as u32, &version_info);

    // ---- 写文件 ----
    let res_path = out_dir.join("version.res");
    std::fs::write(&res_path, &res).ok()?;

    // 同时产出一个 .rc 便于人工查看（不参与编译，仅调试参考）
    let _ = std::fs::write(
        out_dir.join("version_debug.rc"),
        format!(
            "// 仅供查看，实际资源由 build.rs 直接生成 .res\n\
             VS_VERSION_INFO VERSIONINFO\n\
             \x20FILEVERSION {}\n\
             \x20PRODUCTVERSION {}\n\
             \x20VALUE \"CompanyName\", \"{company}\"\n\
             \x20VALUE \"FileDescription\", \"{display_name}\"\n\
             \x20VALUE \"ProductName\", \"{display_name}\"\n\
             \x20VALUE \"FileVersion\", \"{ver_str}\"\n\
             \x20VALUE \"LegalCopyright\", \"{copyright}\"\n",
            ver_str, ver_str
        ),
    );

    println!("cargo:rerun-if-changed=build.rs");

    // ---- 包成 COFF 目标文件 ----
    let obj = wrap_rsrc_in_coff(&res, &reloc_offsets);
    let obj_path = out_dir.join("version_res.o");
    std::fs::write(&obj_path, &obj).ok()?;

    Some(obj_path)
}

/// 构造 52 字节的 VS_FIXEDFILEINFO。
fn build_fixed_file_info(ver: [u16; 4]) -> Vec<u8> {
    let mut v = Vec::with_capacity(52);
    v.extend_from_slice(&0xFE_EF_04_BDu32.to_le_bytes()); // dwSignature
    v.extend_from_slice(&0x0001_0000u32.to_le_bytes()); // dwStrucVersion
    v.extend_from_slice(&(((ver[0] as u32) << 16) | ver[1] as u32).to_le_bytes()); // FileVersionMS
    v.extend_from_slice(&(((ver[2] as u32) << 16) | ver[3] as u32).to_le_bytes()); // FileVersionLS
    v.extend_from_slice(&(((ver[0] as u32) << 16) | ver[1] as u32).to_le_bytes()); // ProductVersionMS
    v.extend_from_slice(&(((ver[2] as u32) << 16) | ver[3] as u32).to_le_bytes()); // ProductVersionLS
    v.extend_from_slice(&0x3Fu32.to_le_bytes()); // dwFileFlagsMask
    v.extend_from_slice(&0u32.to_le_bytes()); // dwFileFlags
    v.extend_from_slice(&0x0004_0004u32.to_le_bytes()); // dwFileOS = VOS_NT_WINDOWS32
    v.extend_from_slice(&0x0000_0001u32.to_le_bytes()); // dwFileType = VFT_APP
    v.extend_from_slice(&0u32.to_le_bytes()); // dwFileSubtype
    v.extend_from_slice(&0u32.to_le_bytes()); // dwFileDateMS
    v.extend_from_slice(&0u32.to_le_bytes()); // dwFileDateLS
    assert_eq!(v.len(), 52);
    v
}

/// 把一个 UTF-16 字符串编码为 UTF-16LE，并以单个 `\0` 结尾。
///
/// 只加一个结尾 `\0`——多余的补齐交给调用方按「绝对位置」统一处理，
/// 否则两处都补会让后续字段错位。
fn utf16z(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() * 2 + 2);
    for u in s.encode_utf16() {
        v.extend_from_slice(&u.to_le_bytes());
    }
    v.extend_from_slice(&0u16.to_le_bytes()); // 结尾 \0
    v
}

/// 构造一个带 Key 的节点（无 Value 或 Value 在 children 里）。
///
/// 对齐规则与 `build_version_root` 一致：**按绝对位置（含 6 字节头）补齐到 4 字节**。
/// 若按 body 自身长度补齐，key 长度恰好是 4 的倍数时就会少补 2 字节，
/// 导致子节点（StringTable）落在错误的偏移上，
/// `VerQueryValue("\StringFileInfo\...")` 全部查不到、属性页空白。
///
/// `wType` 用 1（文本）。`StringFileInfo` / `VarFileInfo` / `StringTable` 这些
/// 容器节点在 windres 产物里都是 `wType=1`；写成 0 会让 Windows 认为节点
/// 不是字符串结构而拒绝解析其子项。
fn build_node(key: &str, value_len: u16, children: &[u8]) -> Vec<u8> {
    const HEADER_LEN: usize = 6; // wLength(2) + wValueLength(2) + wType(2)

    let key_bytes = utf16z(key);
    let mut body = Vec::new();
    body.extend_from_slice(&key_bytes);
    // 按「绝对位置」补齐到 4 字节
    while (HEADER_LEN + body.len()) % 4 != 0 {
        body.push(0);
    }
    body.extend_from_slice(children);

    let mut node = Vec::new();
    let total = (HEADER_LEN + body.len()) as u16;
    node.extend_from_slice(&total.to_le_bytes()); // wLength
    node.extend_from_slice(&value_len.to_le_bytes()); // wValueLength
    node.extend_from_slice(&1u16.to_le_bytes()); // wType = 1 (text)
    node.extend_from_slice(&body);
    node
}

/// 构造 VERSIONINFO 的根节点（`VS_VERSIONINFO`）。
///
/// 头部与子节点一样是 6 字节：`WORD wLength` + `WORD wValueLength` + `WORD wType`。
/// 逐字节核对 `windres` 产物确认（其载荷开头为 `50 01 34 00 00 00`：
/// wLength=0x0150、wValueLength=0x0034、wType=0）。
///
/// szKey 之后补齐到 **4 字节的绝对边界**再放 `VS_FIXEDFILEINFO`。
///
/// 这里最容易错的是「对齐的基准」：szKey 从绝对偏移 6 开始（前面有 6 字节头），
/// 32 字节后到绝对偏移 38，必须补 2 字节让 `VS_FIXEDFILEINFO` 落在绝对偏移 40。
/// 如果按「body 自身长度」去补齐（body=32，32%4==0，于是不补），
/// `VS_FIXEDFILEINFO` 就会落在 38，`dwSignature` 读出来是 0 而不是 0xFEEF04BD，
/// Windows 判定版本资源非法，属性页直接空白——正是本次踩到的坑。
///
/// `HEADER_LEN` 就是把 6 字节头计入对齐基准的修正量。
fn build_version_root(value: &[u8], children: &[u8]) -> Vec<u8> {
    const HEADER_LEN: usize = 6; // wLength(2) + wValueLength(2) + wType(2)

    let key_bytes = utf16z("VS_VERSION_INFO");
    let mut body = Vec::new();
    body.extend_from_slice(&key_bytes);
    // 按「绝对位置」补齐到 4 字节
    while (HEADER_LEN + body.len()) % 4 != 0 {
        body.push(0);
    }
    body.extend_from_slice(value);
    // value 之后同样按绝对位置补齐
    while (HEADER_LEN + body.len()) % 4 != 0 {
        body.push(0);
    }
    body.extend_from_slice(children);

    let mut node = Vec::new();
    let total = (HEADER_LEN + body.len()) as u16;
    node.extend_from_slice(&total.to_le_bytes()); // wLength (WORD)
    node.extend_from_slice(&(value.len() as u16).to_le_bytes()); // wValueLength
    node.extend_from_slice(&0u16.to_le_bytes()); // wType = 0 (binary)
    node.extend_from_slice(&body);
    node
}

/// 构造带 Key 且带 Value 的节点（Value 紧随 Key 之后）。
fn build_node_with_value(key: &str, value_len: u16, value: &[u8], children: &[u8]) -> Vec<u8> {
    let key_bytes = utf16z(key);
    let mut body = Vec::new();
    body.extend_from_slice(&key_bytes);
    // 按「绝对位置」（含 6 字节头）补齐到 4 字节
    while (6 + body.len()) % 4 != 0 {
        body.push(0);
    }
    body.extend_from_slice(value);
    while (6 + body.len()) % 4 != 0 {
        body.push(0);
    }
    body.extend_from_slice(children);

    let mut node = Vec::new();
    let total = (6 + body.len()) as u16;
    node.extend_from_slice(&total.to_le_bytes());
    node.extend_from_slice(&value_len.to_le_bytes());
    node.extend_from_slice(&0u16.to_le_bytes()); // wType = 0（binary，Translation 用）
    node.extend_from_slice(&body);
    node
}

/// 构造一个 `VALUE "key", L"value"` 对应的字符串表条目。
///
/// 两个要点（都踩过坑）：
///   1. `wValueLength` 是**字符个数**（UTF-16 码元数），且**包含结尾的 \0**。
///      写成「不含 \0 的字符数」会让 Windows 少读一个字符；
///      写成字节数则会读错长度，表现为取到的字符串被截断或串到相邻字段。
///   2. key 之后的补齐要按「绝对位置」（含 6 字节头）算到 4 字节边界，
///      否则值会落在错误偏移上。
fn build_string_entry(key: &str, value: &str) -> Vec<u8> {
    let key_bytes = utf16z(key);
    let val_utf16: Vec<u16> = value.encode_utf16().collect();
    // 字符数（含结尾 \0）
    let value_len = (val_utf16.len() + 1) as u16;

    let mut val_bytes = Vec::new();
    for u in &val_utf16 {
        val_bytes.extend_from_slice(&u.to_le_bytes());
    }
    val_bytes.extend_from_slice(&0u16.to_le_bytes()); // 结尾 \0

    let mut body = Vec::new();
    body.extend_from_slice(&key_bytes);
    // 按绝对位置补齐到 4 字节
    while (6 + body.len()) % 4 != 0 {
        body.push(0);
    }
    body.extend_from_slice(&val_bytes);
    while (6 + body.len()) % 4 != 0 {
        body.push(0);
    }

    let mut node = Vec::new();
    let total = (6 + body.len()) as u16;
    node.extend_from_slice(&total.to_le_bytes());
    node.extend_from_slice(&value_len.to_le_bytes());
    node.extend_from_slice(&1u16.to_le_bytes()); // wType = 1 (text)
    node.extend_from_slice(&body);
    node
}

/// 补齐到 4 字节边界。
fn pad4(v: &mut Vec<u8>) {
    while v.len() % 4 != 0 {
        v.push(0);
    }
}

/// 构造 PE 资源树（放进 `.rsrc` 段的真正内容）。
///
/// `.rsrc` 段里装的**不是** `.res` 文件，而是一棵 `IMAGE_RESOURCE_DIRECTORY`
/// 树，共三层。逐字节核对 `windres` 产物后，实际布局如下（偏移相对段首）：
///
/// ```text
///   0x00  第 1 层目录（按类型）  named=0 ids=1
///   0x10    条目：id=0x10(RT_VERSION)、offset=0x80000018 → 指向 0x18
///   0x18  第 2 层目录（按名字）  named=1 ids=0   ← 注意是 named！
///   0x28    条目：name=0x80000048 → 名字串在 0x48、subdir=0x80000030
///   0x30  第 3 层目录（按语言）  named=0 ids=1
///   0x40    条目：id=0x0409、offset=0x68 → 数据条目
///   0x48  名字字符串 "VS_VERSION_INFO"（WORD 长度前缀 + UTF-16 + \0）
///   0x68  数据条目：OffsetToData、Size
///   0x78  VERSIONINFO 本体
/// ```
///
/// 两个必须照做的细节：
///
/// 1. 第 2 层用**命名字符串** `"VS_VERSION_INFO"`，不是数字 ID。
///    版本资源的固定约定就是按这个名字查（`GetFileVersionInfo` 内部如此），
///    换成数字 ID 会直接 1813 失败。
/// 2. 目录条目的偏移、以及数据条目的 `OffsetToData`，都要由链接器修正成绝对
///    RVA（本机为 0xce3000 + 段内偏移），所以必须带重定位，见
///    `wrap_rsrc_in_coff`。
///
/// 返回值第二项是「需要重定位的字段在段内的偏移」列表，供生成 COFF 重定位使用。
fn build_pe_resource_tree(
    res_type: u32,
    res_lang: u32,
    data: &[u8],
) -> (Vec<u8>, Vec<usize>) {
    const DIR_SIZE: usize = 16;
    const ENTRY_SIZE: usize = 8;

    // 各块的偏移
    let l1_off = 0;
    let l2_off = l1_off + DIR_SIZE + ENTRY_SIZE; // 0x18
    let l3_off = l2_off + DIR_SIZE + ENTRY_SIZE; // 0x30
    let data_entry_off = l3_off + DIR_SIZE + ENTRY_SIZE; // 0x48
    let payload_off = data_entry_off + ENTRY_SIZE; // 0x50

    let mut out = Vec::with_capacity(payload_off + data.len() + 32);
    // 需要重定位的字段：只有数据条目的 OffsetToData。
    //
    // 目录条目里的偏移**不能**重定位：那些字段最高位是「是否指向子目录」的
    // 标志位，加段基址会把标志位一起污染（实测得到 0x80ce3018，目录走飞）。
    // 目录内部偏移由 ld 对 `.rsrc` 段的特殊处理负责修正——windres 产出的
    // 对象同样如此（它只有一条 Type=0x0000 的空重定位）。
    let mut reloc_offsets: Vec<usize> = Vec::with_capacity(1);

    // ---- 第 1 层：按类型 ----
    push_dir(&mut out, 0, 1);
    push_entry(&mut out, res_type, 0x8000_0000 | l2_off as u32);

    // ---- 第 2 层：按 ID（数字 1）----
    //
    // 版本资源第 2 层用数字 ID 1。系统自带 exe（如 notepad）就是这个形式；
    // windres 会用命名字符串 "VS_VERSION_INFO"，两种 Windows 都认，
    // 但数字 ID 更简单，没有字符串长度/终止符的坑。
    push_dir(&mut out, 0, 1);
    push_entry(&mut out, 1, 0x8000_0000 | l3_off as u32);

    // ---- 第 3 层：按语言 ----
    push_dir(&mut out, 0, 1);
    push_entry(&mut out, res_lang, data_entry_off as u32); // 最高位 0 → 数据条目

    debug_assert_eq!(out.len(), data_entry_off);

    // ---- 数据条目 ----
    out.extend_from_slice(&(payload_off as u32).to_le_bytes()); // OffsetToData（待重定位）
    reloc_offsets.push(out.len() - 4);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes()); // Size

    debug_assert_eq!(out.len(), payload_off);

    // ---- VERSIONINFO 本体 ----
    out.extend_from_slice(data);

    (out, reloc_offsets)
}

/// 写一个 `IMAGE_RESOURCE_DIRECTORY`（16 字节）。
fn push_dir(out: &mut Vec<u8>, named: u16, ids: u16) {
    out.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    out.extend_from_slice(&0u32.to_le_bytes()); // TimeDateStamp
    out.extend_from_slice(&0u16.to_le_bytes()); // MajorVersion
    out.extend_from_slice(&0u16.to_le_bytes()); // MinorVersion
    out.extend_from_slice(&named.to_le_bytes()); // NumberOfNamedEntries
    out.extend_from_slice(&ids.to_le_bytes()); // NumberOfIdEntries
}

/// 写一个目录条目（8 字节）。
fn push_entry(out: &mut Vec<u8>, id: u32, offset: u32) {
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&offset.to_le_bytes());
}

/// 把 `.rsrc` 数据包成一个 COFF 目标文件，供 GNU ld 链接。
///
/// 结构逐字节对齐 `windres -O coff` 的产物，三部分：
///   1. 20 字节 COFF 文件头（Machine=AMD64、1 个段、Characteristics=0x0004）
///   2. 40 字节段头（名字 `.rsrc`、SizeOfRawData、PointerToRawData、标志 0x40300040）
///   3. 段数据 + 1 个符号表项 + 字符串表
///
/// 符号表不可省略：段头里给出的 `PointerToRawData` 只是「段数据在哪」，
/// 而 ld 需要看到一个指向该段的符号，才会把这个段真正并入输出、并让链接器
/// 把它识别为资源段（否则会被当作无用输入丢弃，表现为资源查不到）。
/// windres 的做法就是往符号表里放一个名为 `.rsrc` 的静态符号
/// （Value=0、SectionNumber=1、StorageClass=3），这里照做。
fn wrap_rsrc_in_coff(rsrc: &[u8], reloc_offsets: &[usize]) -> Vec<u8> {
    const HDR: usize = 20; // COFF 文件头
    const SECT: usize = 40; // 一个段头
    const SYM: usize = 18; // 一个符号表项
    const REL: usize = 10; // 一条重定位记录
    let data_off = HDR + SECT;
    let reloc_count = reloc_offsets.len();

    // 重定位表放在段数据之后
    let rel_off = data_off + rsrc.len();
    let sym_off = rel_off + REL * reloc_count;

    let mut out = Vec::with_capacity(sym_off + SYM + 4);

    // --- 1. COFF 文件头 ---
    out.extend_from_slice(&0x8664u16.to_le_bytes()); // Machine = AMD64
    out.extend_from_slice(&1u16.to_le_bytes()); // NumberOfSections
    out.extend_from_slice(&0u32.to_le_bytes()); // TimeDateStamp
    out.extend_from_slice(&(sym_off as u32).to_le_bytes()); // PointerToSymbolTable
    out.extend_from_slice(&1u32.to_le_bytes()); // NumberOfSymbols = 1
    out.extend_from_slice(&0u16.to_le_bytes()); // SizeOfOptionalHeader
    out.extend_from_slice(&0x0004u16.to_le_bytes()); // Characteristics

    // --- 2. 段头 ---
    let mut name = [0u8; 8];
    name[..5].copy_from_slice(b".rsrc");
    out.extend_from_slice(&name); // Name
    out.extend_from_slice(&0u32.to_le_bytes()); // VirtualSize
    out.extend_from_slice(&0u32.to_le_bytes()); // VirtualAddress
    out.extend_from_slice(&(rsrc.len() as u32).to_le_bytes()); // SizeOfRawData
    out.extend_from_slice(&(data_off as u32).to_le_bytes()); // PointerToRawData
    out.extend_from_slice(&(rel_off as u32).to_le_bytes()); // PointerToRelocations
    out.extend_from_slice(&0u32.to_le_bytes()); // PointerToLinenumbers
    out.extend_from_slice(&(reloc_count as u16).to_le_bytes()); // NumberOfRelocations
    out.extend_from_slice(&0u16.to_le_bytes()); // NumberOfLinenumbers
    // Characteristics：CNT_INITIALIZED_DATA | MEM_READ | MEM_DISCARDABLE
    out.extend_from_slice(&0x4030_0040u32.to_le_bytes());

    // --- 3. 段数据 ---
    out.extend_from_slice(rsrc);

    // --- 4. 重定位记录（每条 10 字节）---
    //
    // 资源目录里**每一个** 32 位偏移字段都要重定位，不只是数据条目的
    // OffsetToData：三层目录条目各自的 OffsetToData 同样是「段内偏移」，
    // 运行期必须是绝对 RVA。漏掉任何一个，Windows 顺着目录走就会读到
    // 无效地址，`GetFileVersionInfoSize` 直接返回 0 并报 1813。
    //
    // 字段顺序：VirtualAddress、SymbolTableIndex、Type
    //
    // Type 用 0x0003 = IMAGE_REL_AMD64_ADDR32NB（32 位 RVA，不含映像基址）。
    // 不能用 0x0002(ADDR32)：那会算出 64 位完整地址，写进 32 位字段溢出，
    // ld 报 `relocation truncated to fit`。资源目录存的就是 RVA，故用 ADDR32NB。
    for &off in reloc_offsets {
        out.extend_from_slice(&(off as u32).to_le_bytes()); // VirtualAddress
        out.extend_from_slice(&0u32.to_le_bytes()); // SymbolTableIndex = 第 0 个符号
        out.extend_from_slice(&0x0003u16.to_le_bytes()); // Type = ADDR32NB
    }

    // --- 5. 符号表：一个名为 ".rsrc" 的静态符号 ---
    let mut sym_name = [0u8; 8];
    sym_name[..5].copy_from_slice(b".rsrc");
    out.extend_from_slice(&sym_name); // Name
    out.extend_from_slice(&0u32.to_le_bytes()); // Value
    out.extend_from_slice(&1i16.to_le_bytes()); // SectionNumber = 1
    out.extend_from_slice(&0u16.to_le_bytes()); // Type
    out.push(3); // StorageClass = IMAGE_SYM_CLASS_STATIC
    out.push(0); // NumberOfAuxSymbols

    // --- 6. 字符串表（至少 4 字节的长度字段） ---
    out.extend_from_slice(&4u32.to_le_bytes());

    out
}








