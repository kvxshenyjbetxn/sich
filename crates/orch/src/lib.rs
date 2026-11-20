//! Sich orchestration: a coordinator agent starts worker agents in new terminals, hands them
//! tasks and exchanges mail with them. Everything here is GPUI-free and daemon-protocol-free:
//! state lives in a small SQLite database, the rest is pure decision logic.

pub mod auto_close;
pub mod cli_parse;
pub mod db;
pub mod delivery;
pub mod dialogs;
pub mod escalation;
pub mod launch;
pub mod model;
pub mod paths;
pub mod preamble;
pub mod watch;

pub use db::Db;
pub use model::*;
