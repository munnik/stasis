// Author: Dustin Pilgrim
// License: GPL-3.0-only

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Run a command (shell string) detached or blocking is runtime policy.
    RunCommand { command: String },

    /// Run a resume command (e.g., dpms on) when activity returns.
    RunResumeCommand { command: String },

    /// Notify the user (runtime decides how: notify-send, dbus notification, etc.)
    Notify {
        message: String,
        icon: Option<String>,
    },

    /// Lock-screen action: run the locker command and (optionally) also lock-session.
    ///
    /// The daemon should run `command` BLOCKING and only consider the lock "ended"
    /// once the process exits.
    RunLockScreen { command: String },

    /// Request system suspend (runtime decides command/system call).
    Suspend,
}
