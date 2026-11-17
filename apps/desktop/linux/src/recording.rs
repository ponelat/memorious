//! Foreground microphone capture; plaintext lives only in anonymous memory files.
use anyhow::{bail, Context, Result};
use std::{
    fs::File,
    io::{Seek, SeekFrom},
    os::fd::{AsRawFd, FromRawFd},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

pub fn memory_file() -> Result<File> {
    let fd = unsafe { libc::memfd_create(c"memorious-audio".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}
pub fn file_path(file: &File) -> String {
    format!("/proc/{}/fd/{}", std::process::id(), file.as_raw_fd())
}

pub struct Recording {
    child: Child,
    pcm: File,
}
impl Recording {
    pub fn start() -> Result<Self> {
        let mut command = Command::new("pw-record");
        command.args([
            "--raw",
            "--rate",
            "16000",
            "--channels",
            "1",
            "--format",
            "s16",
            "--media-role",
            "Communication",
            "-",
        ]);
        Self::start_command(command)
    }
    fn start_command(mut command: Command) -> Result<Self> {
        let pcm = memory_file()?;
        let child = command
            .stdout(pcm.try_clone()?)
            .stderr(Stdio::null())
            .stdin(Stdio::null())
            .spawn()
            .context("Could not start microphone; PipeWire pw-record is required")?;
        let mut recording = Self { child, pcm };
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if !recording.is_running()? {
                bail!("Microphone recording failed; check the input device");
            }
            if recording.pcm.metadata()?.len() > 0 {
                return Ok(recording);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        bail!("Microphone did not supply audio; check the input device")
    }
    pub fn is_running(&mut self) -> Result<bool> {
        Ok(self.child.try_wait()?.is_none())
    }
    pub fn finish(mut self) -> Result<File> {
        if self.is_running()? {
            unsafe {
                libc::kill(self.child.id() as i32, libc::SIGINT);
            }
            let deadline = Instant::now() + Duration::from_secs(3);
            while self.is_running()? && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if self.is_running()? {
                bail!("Microphone did not stop");
            }
        }
        if self.pcm.metadata()?.len() < 3200 {
            bail!("Recording is too short; hold for at least a moment");
        }
        let mut m4a = memory_file()?;
        let mut encoder = Command::new("ffmpeg");
        encoder
            .args([
                "-nostdin", "-v", "error", "-y", "-f", "s16le", "-ar", "16000", "-ac", "1", "-i",
            ])
            .arg(file_path(&self.pcm))
            .args(["-c:a", "aac", "-b:a", "96k", "-f", "ipod"])
            .arg(file_path(&m4a))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = encoder
            .spawn()
            .context("Could not encode audio; ffmpeg is required")?;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Some(status) = child.try_wait()? {
                if !status.success() {
                    bail!("Could not encode the recording as AAC/m4a");
                }
                break;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("Audio encoding timed out");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        m4a.seek(SeekFrom::Start(0))?;
        Ok(m4a)
    }
}
impl Drop for Recording {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Read, os::unix::fs::PermissionsExt};
    fn fake_recorder(script: &str) -> (tempfile::TempDir, Command) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("recorder");
        std::fs::write(&path, format!("#!/usr/bin/env python3\n{script}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        (dir, Command::new(path))
    }
    #[test]
    fn records_aac_in_memory_and_reaps_microphone_on_save() {
        let (_dir, cmd) = fake_recorder("import os, signal, time\nsignal.signal(signal.SIGINT, lambda *_: exit(0))\nos.write(1, b'\\x01\\x00' * 16000)\nwhile True: time.sleep(.01)\n");
        let rec = Recording::start_command(cmd).unwrap();
        let mut file = rec.finish().unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert!(memorious_core::media::is_mp4_family(&bytes));
        assert!(bytes.len() > 100);
    }
    #[test]
    fn failed_microphone_never_reports_recording() {
        let (_dir, cmd) = fake_recorder("exit(1)\n");
        assert!(Recording::start_command(cmd).is_err());
    }
    #[test]
    fn cancel_reaps_the_microphone_process() {
        let (_dir, cmd) = fake_recorder("import os, signal, time\nos.write(1, b'\\x01\\x00' * 16000)\nwhile True: time.sleep(.01)\n");
        let rec = Recording::start_command(cmd).unwrap();
        // The child must be reaped by the recording guard, including window teardown.
        let pid = rec.child.id();
        drop(rec);
        assert_eq!(unsafe { libc::kill(pid as i32, 0) }, -1);
    }
}
