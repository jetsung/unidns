//! unidns 库入口：Web 服务、平台适配器、同步引擎等全部能力。
//! 二进制入口见 `src/main.rs`；开发调试用的演示服务见 `examples/demo_server.rs`。

pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod frontend;
pub mod models;
pub mod providers;
pub mod sync;
pub mod web;
