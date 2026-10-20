//! 程序目录布局：统一决定各类文件该放在哪。
//!
//! ## 为什么要有这个模块
//!
//! 原先每个模块各自算自己的路径（`current_exe().parent().join(...)`），
//! 散在 `backup.rs` / `settings.rs` / `presets.rs` / `main.rs` 四处。
//! 想把运行数据收进子目录就得同时改四个地方，容易漏。
//!
//! 现在只有这里知道目录结构，其他地方一律通过本模块取名。
//!
//! ## 布局
//!
//! ```text
//! <exe 所在目录>/
//! ├─ 中兴光猫控制面板.exe
//! ├─ 命令预设.txt          ← 用户要编辑，刻意留在根目录（好找）
//! ├─ 命令预设_f650.txt     ← 同上
//! └─ 数据/
//!    ├─ 连接设置.txt
//!    ├─ 配置备份/
//!    └─ 运行日志/
//! ```
//!
//! **预设文件留在根目录**是有意的：那是用户会主动去编辑的东西，
//! 埋进 `数据/` 里反而不好找。其余都是程序自动产生的运行数据，
//! 收进 `数据/` 让根目录保持干净。
//!
//! ## 为什么基于 exe 而不是当前工作目录
//!
//! 用户双击 exe 时，工作目录可能是任意位置（尤其从开始菜单或
//! 文件关联启动时）。按工作目录找文件会让人困惑「文件到底该放哪」，
//! 也会出现「换个方式启动就读不到设置」的怪现象。

use std::path::PathBuf;

/// 存放程序自动产生数据的子目录名。
pub const DATA_DIR: &str = "数据";

/// exe 所在目录。
///
/// 取不到时返回 None（极罕见），调用方应优雅降级而不是 panic。
pub fn exe_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.to_path_buf())
}

/// 运行数据根目录 `<exe目录>/数据`。
///
/// **不创建目录**——只算路径。需要写入的调用方负责 `create_dir_all`，
/// 因为只读场景（如加载设置）不该有副作用。
pub fn data_dir() -> Option<PathBuf> {
    Some(exe_dir()?.join(DATA_DIR))
}

/// 确保运行数据目录存在，返回其路径。
pub fn ensure_data_dir() -> Option<PathBuf> {
    let d = data_dir()?;
    std::fs::create_dir_all(&d).ok()?;
    Some(d)
}

/// `数据/配置备份`
pub fn backup_dir() -> Option<PathBuf> {
    Some(data_dir()?.join("配置备份"))
}

/// `数据/运行日志`
pub fn log_dir() -> Option<PathBuf> {
    Some(data_dir()?.join("运行日志"))
}

/// `数据/连接设置.txt`
pub fn settings_file() -> Option<PathBuf> {
    Some(data_dir()?.join("连接设置.txt"))
}

/// 原子写：先写 `<名>.part`，`sync_all` 落盘后再改名。
///
/// 直接 `write` 时进程在写盘途中被强杀（用户以为卡死点了结束任务、或断电）
/// 会留下一个**被截断的文件**。备份模块早就改成了这套写法，
/// 而连接设置与外观设置还是裸写 —— 同一类问题两种做法，
/// 后果虽轻（解析宽容、坏文件回落默认值），但「保存过的地址下次启动
/// 变回 192.168.1.1」同样会让人以为是程序坏了。
///
/// 没有父目录时退化成直接写：测试里常传相对路径，
/// 不该因为 `parent()` 为空就整体失败。
pub fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty());
    let Some(dir) = dir else {
        return std::fs::write(path, bytes);
    };
    std::fs::create_dir_all(dir)?;
    let file_name = path
        .file_name()
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "路径没有文件名")
        })?;
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(".part");
    let tmp = path.with_file_name(tmp_name);
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    // rename 在同一分区内是原子的：要么看到完整的新文件，要么还是旧的。
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// exe 同目录（预设文件放这里，方便用户编辑）。
pub fn preset_dir() -> Option<PathBuf> {
    exe_dir()
}

/// 把旧版遗留在根目录的运行数据搬到 `数据/` 下。
///
/// 升级时用户根目录里已经有 `连接设置.txt` / `配置备份/` / `运行日志/`，
/// 直接改用新路径会让用户觉得「设置丢了」「日志没了」。
/// 这里做一次性搬迁：**只在目标不存在时搬**，绝不覆盖新数据。
///
/// 返回搬迁成功的条目名，供日志显示。
pub fn migrate_legacy_layout() -> Vec<String> {
    let mut moved = Vec::new();
    let (Some(exe_dir), Some(data)) = (exe_dir(), data_dir()) else {
        return moved;
    };

    // 待搬迁的：旧路径 → 新路径
    let candidates: [(&str, PathBuf); 3] = [
        ("连接设置.txt", data.join("连接设置.txt")),
        ("配置备份", data.join("配置备份")),
        ("运行日志", data.join("运行日志")),
    ];

    for (old_name, new_path) in candidates {
        let old = exe_dir.join(old_name);
        if !old.exists() {
            continue;
        }
        // 目标已存在就不动：新数据优先，避免覆盖用户当前在用的东西
        if new_path.exists() {
            continue;
        }
        if std::fs::create_dir_all(&data).is_err() {
            break;
        }
        // 目录用 rename 一次搬完（同盘内是原子操作）
        if std::fs::rename(&old, &new_path).is_ok() {
            moved.push(old_name.to_string());
        }
    }
    moved
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    #[test]
    fn 数据目录在exe目录下() {
        if let (Some(exe), Some(data)) = (exe_dir(), data_dir()) {
            assert_eq!(data.parent().unwrap(), exe);
            assert_eq!(data.file_name().unwrap(), DATA_DIR);
        }
    }

    #[test]
    fn 各子路径都在数据目录下() {
        let Some(data) = data_dir() else { return };
        for p in [backup_dir(), log_dir(), settings_file()].into_iter().flatten() {
            assert!(
                p.starts_with(&data),
                "{} 不在数据目录下",
                p.display()
            );
            // 不能直接落在数据目录根上（那还是把根目录弄乱了）
            assert_ne!(p, data);
        }
    }

    #[test]
    fn 预设目录是exe目录而非数据目录() {
        // 预设要留给用户编辑，必须在根目录好找
        assert_eq!(preset_dir(), exe_dir());
        if let (Some(p), Some(d)) = (preset_dir(), data_dir()) {
            assert_ne!(p, d, "预设不该被收进数据目录");
        }
    }

    /// 原子写：写完之后目标文件是完整的，且不留下 .part 残留。
    ///
    /// 关键在「不留残留」：以前备份模块的 .part 清理是单独测的，
    /// 而设置 / 外观直接裸写，被强杀会留下截断文件。
    #[test]
    fn 原子写留下完整文件且不残留临时文件() {
        let dir = std::env::temp_dir().join(format!("zte_atomic_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("连接设置.txt");

        // 目录不存在也要能写（会自己建）
        write_atomic(&path, b"address=10.0.0.9\n").expect("写盘失败");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "address=10.0.0.9\n");

        // 覆盖写同样生效
        //
        // 先放一个同名的 .part 占位文件：如果实现真的走「临时文件 → 改名」，
        // 这个占位会被 rename 覆盖掉；如果退化成直接写目标文件，
        // 占位文件会原样留着 —— 这条断言就是在验「有没有真的用临时文件」。
        let part = dir.join("连接设置.txt.part");
        std::fs::write(&part, "占位残留").unwrap();
        write_atomic(&path, b"address=10.0.0.10\n").expect("覆盖写失败");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "address=10.0.0.10\n");
        assert!(!part.exists(), "临时文件应当被改名消耗掉，不该残留");

        // 目录里只该有目标文件，不能有 .part
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["连接设置.txt".to_string()], "不该留下临时文件：{names:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 路径不重复() {
        let paths: Vec<PathBuf> = [backup_dir(), log_dir(), settings_file()]
            .into_iter()
            .flatten()
            .collect();
        for i in 0..paths.len() {
            for j in (i + 1)..paths.len() {
                assert_ne!(paths[i], paths[j], "路径重复: {}", paths[i].display());
            }
        }
    }
}
