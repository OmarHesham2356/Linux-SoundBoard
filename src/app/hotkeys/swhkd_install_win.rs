//! The swhkd installer is a Linux-only feature. The UI shares these types on
//! every platform, so Windows keeps the same surface with a clear refusal.

pub const SWHKD_UPSTREAM_INSTALL_URL: &str =
    "https://github.com/waycrate/swhkd/blob/main/INSTALL.md";

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SwhkdInstallState {
    Idle,
    Checking,
    Installing,
    Verifying,
    Completed,
    Failed,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum SwhkdInstallErrorKind {
    UnsupportedDistro,
    MissingPkexec,
    MissingHelper,
    PrivilegeDenied,
    CommandFailed,
    VerificationFailed,
}

#[derive(Debug, Clone)]
pub struct SwhkdInstallError {
    pub kind: SwhkdInstallErrorKind,
    pub summary: String,
    pub details: String,
    pub state: SwhkdInstallState,
}

impl std::fmt::Display for SwhkdInstallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.details.is_empty() {
            write!(f, "{}", self.summary)
        } else {
            write!(f, "{}\n\n{}", self.summary, self.details)
        }
    }
}

#[derive(Debug, Clone)]
pub struct SwhkdInstallReport {
    pub summary: String,
    pub details: String,
    pub states: Vec<SwhkdInstallState>,
}

pub fn should_offer_swhkd_install(_raw_error: &str) -> bool {
    false
}

pub fn uinput_unavailable() -> bool {
    false
}

pub fn manual_swhkd_install_commands() -> String {
    String::new()
}

pub fn install_swhkd_native_detailed(
    _enable_uinput: bool,
) -> Result<SwhkdInstallReport, SwhkdInstallError> {
    Err(SwhkdInstallError {
        kind: SwhkdInstallErrorKind::UnsupportedDistro,
        summary: "swhkd is not used on Windows".to_string(),
        details: String::new(),
        state: SwhkdInstallState::Failed,
    })
}
