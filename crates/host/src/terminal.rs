//! PTYs survive client disconnection. Buffered output is bounded and sequence numbered.
use anyhow::{bail, Result};
use portable_pty::{native_pty_system, Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};

pub struct Terminal {
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    output: Mutex<(u64, VecDeque<Value>, bool)>,
    stopped: AtomicBool,
    shell_pid: Option<u32>,
    wsl_lease: Option<crate::wsl::WslLease>,
}
#[derive(Default)]
pub struct Terminals(Mutex<HashMap<String, Arc<Terminal>>>);
impl Drop for Terminals {
    fn drop(&mut self) {
        self.stop_all();
    }
}
impl Terminals {
    pub fn open(&self, id: &str, root: &std::path::Path) -> Result<Arc<Terminal>> {
        self.open_target(
            id,
            &crate::models::ExecutionTarget::Native,
            root.to_str()
                .ok_or_else(|| anyhow::anyhow!("terminal path is not UTF-8"))?,
        )
    }
    pub fn open_target(
        &self,
        id: &str,
        target: &crate::models::ExecutionTarget,
        workspace: &str,
    ) -> Result<Arc<Terminal>> {
        let mut all = self.0.lock().unwrap();
        if let Some(t) = all.get(id) {
            if !t.stopped.load(Ordering::Acquire) {
                return Ok(t.clone());
            }
        }
        all.remove(id);
        if all.len() >= 32 {
            bail!("terminal capacity reached");
        }
        let pair = native_pty_system().openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let shell = if cfg!(windows) {
            "powershell.exe".to_owned()
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
        };
        let (mut cmd, wsl_lease) = match target {
            crate::models::ExecutionTarget::Native => {
                let mut cmd = CommandBuilder::new(shell);
                cmd.cwd(workspace);
                (cmd, None)
            }
            crate::models::ExecutionTarget::Wsl { .. } => {
                let spec = crate::wsl::terminal_spec(target, workspace)?;
                let mut cmd = CommandBuilder::new(spec.program);
                cmd.args(spec.args);
                spec.lease.start_renewal();
                (cmd, Some(spec.lease))
            }
        };
        cmd.env("TERM", "xterm-256color");
        let child = pair.slave.spawn_command(cmd)?;
        let killer = child.clone_killer();
        let shell_pid = child.process_id();
        drop(pair.slave);
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let terminal = Arc::new(Terminal {
            master: Mutex::new(pair.master),
            writer: Mutex::new(writer),
            killer: Mutex::new(killer),
            output: Mutex::new((0, VecDeque::new(), false)),
            stopped: AtomicBool::new(false),
            shell_pid,
            wsl_lease,
        });
        let sink = terminal.clone();
        std::thread::spawn(move || read_output(sink, reader, child));
        all.insert(id.into(), terminal.clone());
        Ok(terminal)
    }
    pub fn stop(&self, id: &str) {
        let terminal = self.0.lock().unwrap().remove(id);
        if let Some(t) = terminal {
            t.stop();
        }
    }
    pub fn stop_all(&self) {
        let all = std::mem::take(&mut *self.0.lock().unwrap());
        for t in all.values() {
            t.stop();
        }
    }
}
impl Terminal {
    fn push(&self, mut v: Value) {
        let mut output = self.output.lock().unwrap();
        output.0 += 1;
        v["sequence"] = json!(output.0);
        output.1.push_back(v);
        while output.1.len() > 512 {
            output.1.pop_front();
        }
    }
    pub fn events(&self, after: u64) -> Vec<Value> {
        let out = self.output.lock().unwrap();
        let mut events = Vec::new();
        if let Some(first) = out.1.front() {
            let first = first["sequence"].as_u64().unwrap_or(0);
            if first > after + 1 {
                events.push(json!({"type":"gap","sequence":first-1,"message":"Older terminal output expired"}));
            }
        }
        events.extend(
            out.1
                .iter()
                .filter(|v| v["sequence"].as_u64().unwrap_or(0) > after)
                .cloned(),
        );
        events
    }
    pub fn input(&self, data: &[u8]) -> Result<()> {
        if data.len() > 64 * 1024 {
            bail!("terminal input too large");
        }
        let mut w = self.writer.lock().unwrap();
        if self.stopped.load(Ordering::Acquire) {
            bail!("terminal stopped");
        }
        w.write_all(data)?;
        w.flush()?;
        Ok(())
    }
    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        if !(1..=500).contains(&cols) || !(1..=300).contains(&rows) {
            bail!("invalid terminal size");
        }
        let master = self.master.lock().unwrap();
        if self.stopped.load(Ordering::Acquire) {
            bail!("terminal stopped");
        }
        master.resize(PtySize {
            cols,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        if let Some(lease) = &self.wsl_lease {
            lease.resize(rows, cols);
        }
        Ok(())
    }
    fn stop(&self) {
        if self.stopped.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Some(lease) = &self.wsl_lease {
            lease.request_cancel();
        }
        #[cfg(unix)]
        if let Some(pid) = self.shell_pid {
            use nix::{
                sys::signal::{kill, killpg, Signal},
                unistd::{getpgrp, Pid},
            };
            // Stop the shell before discovering children so it cannot launch new jobs
            // while its foreground and background process groups are being removed.
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGSTOP);
            let children = descendants(pid);
            for child in &children {
                let _ = kill(Pid::from_raw(*child as i32), Signal::SIGSTOP);
            }
            let foreground = self.master.lock().unwrap().process_group_leader();
            for group in [foreground, Some(pid as i32)].into_iter().flatten() {
                if group > 1 && group != getpgrp().as_raw() {
                    let _ = killpg(Pid::from_raw(group), Signal::SIGKILL);
                }
            }
            for child in children.into_iter().rev() {
                let _ = kill(Pid::from_raw(child as i32), Signal::SIGKILL);
            }
            let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
        }
        #[cfg(windows)]
        if let Some(pid) = self.shell_pid {
            // taskkill traverses the Windows process tree, including jobs launched
            // outside the shell's foreground console process.
            if let Ok(mut child) = std::process::Command::new("taskkill.exe")
                .args(["/PID", &pid.to_string(), "/T", "/F"])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
                while matches!(child.try_wait(), Ok(None)) && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        let _ = self.killer.lock().unwrap().kill();
        self.push(json!({"type":"stopped"}));
    }
}
#[cfg(unix)]
fn descendants(root: u32) -> Vec<u32> {
    let Ok(output) = std::process::Command::new("ps")
        .args(["-axo", "pid=,ppid="])
        .output()
    else {
        return Vec::new();
    };
    let rows = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut values = line.split_whitespace();
            Some((
                values.next()?.parse::<u32>().ok()?,
                values.next()?.parse::<u32>().ok()?,
            ))
        })
        .collect::<Vec<_>>();
    let mut result = Vec::new();
    let mut frontier = vec![root];
    for _ in 0..64 {
        let mut next = Vec::new();
        for (pid, parent) in &rows {
            if frontier.contains(parent) && *pid != root && !result.contains(pid) {
                result.push(*pid);
                next.push(*pid);
            }
        }
        if next.is_empty() {
            break;
        }
        frontier = next;
    }
    result
}
fn read_output(
    t: Arc<Terminal>,
    mut reader: Box<dyn Read + Send>,
    mut child: Box<dyn Child + Send + Sync>,
) {
    use base64::Engine;
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf){Ok(0)|Err(_)=>break,Ok(n)=>t.push(json!({"type":"output","data":base64::engine::general_purpose::STANDARD.encode(&buf[..n])}))}
    }
    let code = child.wait().map(|s| s.exit_code()).ok();
    t.stopped.store(true, Ordering::Release);
    t.push(json!({"type":"exit","code":code}));
    t.output.lock().unwrap().2 = true;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn stop_terminates_background_jobs_and_retained_handles() {
        let dir = tempfile::tempdir().unwrap();
        let terminals = Terminals::default();
        let terminal = terminals.open("test", dir.path()).unwrap();
        terminal
            .input(b"sleep 120 & echo $! > child.pid\n")
            .unwrap();
        let path = dir.path().join("child.pid");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let pid = loop {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    break pid;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "shell did not launch background job"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(descendants(terminal.shell_pid.unwrap()).contains(&pid));
        terminals.stop("test");
        assert!(terminal.input(b"echo should-not-run\n").is_err());
        assert!(terminal.resize(80, 24).is_err());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let output = std::process::Command::new("ps")
                .args(["-p", &pid.to_string(), "-o", "stat="])
                .output()
                .unwrap();
            let status = String::from_utf8_lossy(&output.stdout);
            if !output.status.success()
                || status.trim().is_empty()
                || status.trim().starts_with('Z')
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "background job survived terminal stop: {pid}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
