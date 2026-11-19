//! `sich hook <agent>` (the command agent CLIs run on their hook events) and the installer that
//! registers it in Claude Code's `settings.json`.

pub mod claude_install;
mod hook;

pub use hook::{
    HookEnv, HookOutcome, MAX_STDIN_BYTES, append_log, hook_main, log_path, read_capped, run_hook,
    send_to_daemon,
};
