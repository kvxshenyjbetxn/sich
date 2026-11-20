//! `sich hook <agent>` (the command agent CLIs run on their hook events) and the installer that
//! registers it in Claude Code's `settings.json`.

pub mod claude_install;
mod codex_app_server;
pub mod codex_install;
pub mod codex_trust;
mod hook;
pub mod omp_install;
pub mod statusline;
pub mod usage;

pub use hook::{
    HookEnv, HookOutcome, MAX_STDIN_BYTES, append_log, hook_main, log_path, read_capped, run_hook,
    send_to_daemon,
};
