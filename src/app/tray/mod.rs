//! System tray integration.
//!
//! Linux exports a StatusNotifierItem plus its dbusmenu; Windows installs a
//! `Shell_NotifyIcon` icon from a message-only window. Both report the same
//! actions, so the UI layer is platform-independent.

pub(crate) mod menu;
pub(crate) mod payload;

pub(crate) use menu::MenuAction;
pub(crate) use payload::MenuItem;

#[cfg(target_os = "linux")]
mod service;
#[cfg(windows)]
mod win32;

#[cfg(target_os = "linux")]
pub(crate) use service::{TrayAction, TrayService};
#[cfg(windows)]
pub(crate) use win32::{TrayAction, TrayService};
