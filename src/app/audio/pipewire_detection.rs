#[cfg(target_os = "linux")]
use log::{info, warn};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
use super::command_runner::{CommandRunner, SystemCommandRunner};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PipeWireStatus {
    pub available: bool,
    pub version: Option<String>,
    pub error: Option<String>,
}

#[cfg(target_os = "linux")]
pub fn check_pipewire() -> PipeWireStatus {
    check_pipewire_with_runner(&SystemCommandRunner)
}

/// Windows does not run PipeWire: playback goes straight to WASAPI and a
/// virtual microphone comes from an installed virtual audio cable. Nothing is
/// missing on that platform, so detection reports success and the UI never asks
/// the user to install PipeWire.
#[cfg(windows)]
pub fn check_pipewire() -> PipeWireStatus {
    PipeWireStatus {
        available: true,
        version: None,
        error: None,
    }
}

#[cfg(target_os = "linux")]
fn check_pipewire_with_runner(runner: &impl CommandRunner) -> PipeWireStatus {
    if let Ok(output) = runner.run("pw-cli", &["info", "0"]) {
        if output.success {
            info!("PipeWire detected via pw-cli");
            return PipeWireStatus {
                available: true,
                version: get_pipewire_version(runner),
                error: None,
            };
        }
    }

    if let Ok(output) = runner.run("pgrep", &["-x", "pipewire"]) {
        if output.success {
            info!("PipeWire process detected");
            return PipeWireStatus {
                available: true,
                version: get_pipewire_version(runner),
                error: None,
            };
        }
    }

    let socket_path = format!("{}/pipewire-0", pipewire_runtime_dir());
    if std::path::Path::new(&socket_path).exists() {
        info!("PipeWire socket detected at {}", socket_path);
        return PipeWireStatus {
            available: true,
            version: get_pipewire_version(runner),
            error: None,
        };
    }

    warn!("PipeWire not detected");
    PipeWireStatus {
        available: false,
        version: None,
        error: Some("PipeWire is not running. Please install and start PipeWire.".to_string()),
    }
}

#[cfg(target_os = "linux")]
fn pipewire_runtime_dir() -> String {
    #[cfg(target_os = "linux")]
    {
        std::env::var("XDG_RUNTIME_DIR")
            .unwrap_or_else(|_| format!("/run/user/{}", nix::unistd::Uid::current()))
    }
    #[cfg(not(target_os = "linux"))]
    {
        String::new()
    }
}

#[cfg(target_os = "linux")]
fn get_pipewire_version(runner: &impl CommandRunner) -> Option<String> {
    if let Ok(output) = runner.run("pipewire", &["--version"]) {
        if output.success {
            for line in output.stdout.lines() {
                if line.contains("pipewire") {
                    if let Some(version) = line.split_whitespace().last() {
                        return Some(version.to_string());
                    }
                }
            }
        }
    }
    None
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::collections::HashMap;
    use std::io;

    use super::*;
    use crate::audio::command_runner::CommandOutput;

    struct FakeRunner {
        responses: HashMap<(String, Vec<String>), CommandOutput>,
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, program: &str, args: &[&str]) -> io::Result<CommandOutput> {
            self.responses
                .get(&(
                    program.to_string(),
                    args.iter().map(|arg| (*arg).to_string()).collect(),
                ))
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing command"))
        }
    }

    #[test]
    fn detection_prefers_pw_cli() {
        let mut responses = HashMap::new();
        responses.insert(
            (
                "pw-cli".to_string(),
                vec!["info".to_string(), "0".to_string()],
            ),
            CommandOutput {
                success: true,
                stdout: String::new(),
                stderr: String::new(),
            },
        );
        responses.insert(
            ("pipewire".to_string(), vec!["--version".to_string()]),
            CommandOutput {
                success: true,
                stdout: "pipewire: 1.2.3".to_string(),
                stderr: String::new(),
            },
        );

        let status = check_pipewire_with_runner(&FakeRunner { responses });
        assert!(status.available);
        assert_eq!(status.version.as_deref(), Some("1.2.3"));
    }
}
