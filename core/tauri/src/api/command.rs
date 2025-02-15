// Copyright 2019-2021 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use std::{
  collections::HashMap,
  io::{BufRead, BufReader, Write},
  path::PathBuf,
  process::{Command as StdCommand, Stdio},
  sync::Arc,
};

#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

use crate::async_runtime::{channel, spawn, Receiver, RwLock};
use os_pipe::{pipe, PipeWriter};
use serde::Serialize;
use shared_child::SharedChild;
use tauri_utils::platform;

/// Payload for the `Terminated` command event.
#[derive(Debug, Clone, Serialize)]
pub struct TerminatedPayload {
  /// Exit code of the process.
  pub code: Option<i32>,
  /// If the process was terminated by a signal, represents that signal.
  pub signal: Option<i32>,
}

/// A event sent to the command callback.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", content = "payload")]
pub enum CommandEvent {
  /// Stderr line.
  Stderr(String),
  /// Stdout line.
  Stdout(String),
  /// An error happened.
  Error(String),
  /// Command process terminated.
  Terminated(TerminatedPayload),
}

macro_rules! get_std_command {
  ($self: ident) => {{
    let mut command = StdCommand::new($self.program);
    command.args(&$self.args);
    command.stdout(Stdio::piped());
    command.stdin(Stdio::piped());
    command.stderr(Stdio::piped());
    if $self.env_clear {
      command.env_clear();
    }
    command.envs($self.env);
    if let Some(current_dir) = $self.current_dir {
      command.current_dir(current_dir);
    }
    #[cfg(windows)]
    command.creation_flags(CREATE_NO_WINDOW);
    command
  }};
}

/// API to spawn commands.
pub struct Command {
  program: String,
  args: Vec<String>,
  env_clear: bool,
  env: HashMap<String, String>,
  current_dir: Option<PathBuf>,
}

/// Child spawned.
pub struct CommandChild {
  inner: Arc<SharedChild>,
  stdin_writer: PipeWriter,
}

impl CommandChild {
  /// Write to process stdin.
  pub fn write(&mut self, buf: &[u8]) -> crate::api::Result<()> {
    self.stdin_writer.write_all(buf)?;
    Ok(())
  }

  /// Send a kill signal to the child.
  pub fn kill(self) -> crate::api::Result<()> {
    self.inner.kill()?;
    Ok(())
  }

  /// Returns the process pid.
  pub fn pid(&self) -> u32 {
    self.inner.id()
  }
}

#[cfg(not(windows))]
fn relative_command_path(command: String) -> crate::Result<String> {
  match std::env::current_exe()?.parent() {
    Some(exe_dir) => Ok(format!(
      "{}/{}",
      exe_dir.to_string_lossy().to_string(),
      command
    )),
    None => Err(super::Error::Command("Could not evaluate executable dir".to_string()).into()),
  }
}

#[cfg(windows)]
fn relative_command_path(command: String) -> crate::Result<String> {
  match std::env::current_exe()?.parent() {
    Some(exe_dir) => Ok(format!(
      "{}/{}.exe",
      exe_dir.to_string_lossy().to_string(),
      command
    )),
    None => Err(super::Error::Command("Could not evaluate executable dir".to_string()).into()),
  }
}

impl Command {
  /// Creates a new Command for launching the given program.
  pub fn new<S: Into<String>>(program: S) -> Self {
    Self {
      program: program.into(),
      args: Default::default(),
      env_clear: false,
      env: Default::default(),
      current_dir: None,
    }
  }

  /// Creates a new Command for launching the given sidecar program.
  pub fn new_sidecar<S: Into<String>>(program: S) -> crate::Result<Self> {
    let program = format!(
      "{}-{}",
      program.into(),
      platform::target_triple().expect("unsupported platform")
    );
    Ok(Self::new(relative_command_path(program)?))
  }

  /// Append args to the command.
  pub fn args<I, S>(mut self, args: I) -> Self
  where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
  {
    for arg in args {
      self.args.push(arg.as_ref().to_string());
    }
    self
  }

  /// Clears the entire environment map for the child process.
  pub fn env_clear(mut self) -> Self {
    self.env_clear = true;
    self
  }

  /// Adds or updates multiple environment variable mappings.
  pub fn envs(mut self, env: HashMap<String, String>) -> Self {
    self.env = env;
    self
  }

  /// Sets the working directory for the child process.
  pub fn current_dir(mut self, current_dir: PathBuf) -> Self {
    self.current_dir.replace(current_dir);
    self
  }

  /// Spawns the command.
  pub fn spawn(self) -> crate::api::Result<(Receiver<CommandEvent>, CommandChild)> {
    let mut command = get_std_command!(self);
    let (stdout_reader, stdout_writer) = pipe()?;
    let (stderr_reader, stderr_writer) = pipe()?;
    let (stdin_reader, stdin_writer) = pipe()?;
    command.stdout(stdout_writer);
    command.stderr(stderr_writer);
    command.stdin(stdin_reader);

    let shared_child = SharedChild::spawn(&mut command)?;
    let child = Arc::new(shared_child);
    let child_ = child.clone();
    let guard = Arc::new(RwLock::new(()));

    let (tx, rx) = channel(1);

    let tx_ = tx.clone();
    let guard_ = guard.clone();
    spawn(async move {
      let _lock = guard_.read().await;
      let reader = BufReader::new(stdout_reader);
      for line in reader.lines() {
        let _ = match line {
          Ok(line) => tx_.send(CommandEvent::Stdout(line)).await,
          Err(e) => tx_.send(CommandEvent::Error(e.to_string())).await,
        };
      }
    });

    let tx_ = tx.clone();
    let guard_ = guard.clone();
    spawn(async move {
      let _lock = guard_.read().await;
      let reader = BufReader::new(stderr_reader);
      for line in reader.lines() {
        let _ = match line {
          Ok(line) => tx_.send(CommandEvent::Stderr(line)).await,
          Err(e) => tx_.send(CommandEvent::Error(e.to_string())).await,
        };
      }
    });

    spawn(async move {
      let _ = match child_.wait() {
        Ok(status) => {
          guard.write().await;
          tx.send(CommandEvent::Terminated(TerminatedPayload {
            code: status.code(),
            #[cfg(windows)]
            signal: None,
            #[cfg(unix)]
            signal: status.signal(),
          }))
          .await
        }
        Err(e) => {
          guard.write().await;
          tx.send(CommandEvent::Error(e.to_string())).await
        }
      };
    });

    Ok((
      rx,
      CommandChild {
        inner: child,
        stdin_writer,
      },
    ))
  }
}

// tests for the commands functions.
#[cfg(test)]
mod test {
  use super::*;

  #[cfg(not(windows))]
  #[test]
  fn test_cmd_output() {
    // create a command to run cat.
    let cmd = Command::new("cat").args(&["test/api/test.txt"]);
    let (mut rx, _) = cmd.spawn().unwrap();

    crate::async_runtime::block_on(async move {
      while let Some(event) = rx.recv().await {
        match event {
          CommandEvent::Terminated(payload) => {
            assert_eq!(payload.code, Some(0));
          }
          CommandEvent::Stdout(line) => {
            assert_eq!(line, "This is a test doc!".to_string());
          }
          _ => {}
        }
      }
    });
  }

  #[cfg(not(windows))]
  #[test]
  // test the failure case
  fn test_cmd_fail() {
    let cmd = Command::new("cat").args(&["test/api/"]);
    let (mut rx, _) = cmd.spawn().unwrap();

    crate::async_runtime::block_on(async move {
      while let Some(event) = rx.recv().await {
        match event {
          CommandEvent::Terminated(payload) => {
            assert_eq!(payload.code, Some(1));
          }
          CommandEvent::Stderr(line) => {
            assert_eq!(line, "cat: test/api/: Is a directory".to_string());
          }
          _ => {}
        }
      }
    });
  }
}
