//! 库入口：把协议、解析、路径与预设等模块暴露出去，供 `tests/` 下的集成测试复用。
//!
//! 之所以拆成 lib + bin：`telnet.rs` 的协议实现和 `presets.rs` 的命令列表
//! 都需要被端到端测试直接驱动（连假光猫、跑真实命令序列），
//! 而 binary crate 无法被 `use`。
pub mod appearance;
pub mod backup;
pub mod config;
pub mod dbcli;
pub mod model;
pub mod passwords;
pub mod paths;
pub mod presets;
pub mod settings;
pub mod telnet;
