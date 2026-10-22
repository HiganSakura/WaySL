//! Optional Waypipe process launched alongside the renderer.

use bevy::prelude::Resource;
use std::io;
use std::process::{Child, Command};

/// Owns an optional child process and terminates it on drop.
#[derive(Debug, Default, Resource)]
pub struct WaypipeProcess {
    command_line: Option<String>,
    child: Option<Child>,
}

impl WaypipeProcess {
    /// Creates the guard for an optional `WAYSL_WAYPIPE_COMMAND` value.
    #[must_use]
    pub fn new(command_line: Option<String>) -> Self {
        let command_line = command_line.filter(|line| !line.trim().is_empty());
        Self {
            command_line,
            child: None,
        }
    }

    /// Spawns the configured command, if any.
    ///
    /// An unset command is not an error; a malformed one is reported by the
    /// caller and never retried.
    pub fn start(&mut self) -> io::Result<()> {
        if self.child.is_some() {
            return Ok(());
        }
        let Some(command_line) = self.command_line.as_deref() else {
            return Ok(());
        };
        let parts = parse(command_line)?;
        let (program, args) = parts
            .split_first()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "empty waypipe command"))?;
        self.child = Some(Command::new(program).args(args).spawn()?);
        Ok(())
    }
}

impl Drop for WaypipeProcess {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Splits a command line into program and arguments.
fn parse(command_line: &str) -> io::Result<Vec<String>> {
    let parts: Vec<String> = command_line
        .split_whitespace()
        .map(ToString::to_string)
        .collect();
    if parts.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty waypipe command",
        ));
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_splits_program_and_arguments() {
        assert_eq!(
            parse("waypipe --socket /tmp/x client").expect("parse"),
            ["waypipe", "--socket", "/tmp/x", "client"]
        );
    }

    #[test]
    fn parse_rejects_empty() {
        assert!(parse("   ").is_err());
    }

    #[test]
    fn unset_command_is_not_an_error() {
        let mut process = WaypipeProcess::new(None);
        assert!(process.start().is_ok());
        assert!(process.child.is_none());
    }

    #[test]
    fn empty_command_is_treated_as_unset() {
        let mut process = WaypipeProcess::new(Some("   ".to_string()));
        assert!(process.start().is_ok());
    }
}
