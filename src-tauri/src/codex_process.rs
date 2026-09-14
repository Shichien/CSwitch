use std::error::Error;
use std::path::Path;

const CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);

pub(crate) fn after_closed_with<T, C, F>(close: C, action: F) -> Result<T, Box<dyn Error>>
where
    C: FnOnce() -> Result<bool, Box<dyn Error>>,
    F: FnOnce() -> Result<T, Box<dyn Error>>,
{
    close()?;
    action()
}

pub(crate) fn close_if_running() -> Result<bool, Box<dyn Error>> {
    if cfg!(test) {
        return Ok(false);
    }
    platform::close_if_running()
}

fn is_codex_desktop_process(name: &str, executable: &Path) -> bool {
    let normalized = executable
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();
    let lower_name = name.to_lowercase();

    let macos_app = (lower_name == "codex"
        && normalized.contains("/codex.app/contents/macos/codex"))
        || (lower_name == "chatgpt" && normalized.contains("/chatgpt.app/contents/macos/chatgpt"));
    let windows_store_app = (lower_name == "codex.exe" || lower_name == "chatgpt.exe")
        && normalized.contains("/program files/windowsapps/openai.codex_");
    let windows_app_server =
        lower_name == "codex.exe" && normalized.contains("/appdata/local/openai/codex/bin/");
    let windows_app = windows_store_app
        || windows_app_server
        || (lower_name == "codex.exe"
            && (normalized.contains("/program files/codex/")
                || normalized.contains("/appdata/local/codex/")
                || normalized.contains("/appdata/local/programs/codex/")));
    let linux_app = (lower_name == "codex" || lower_name == "codex-desktop")
        && (normalized.starts_with("/opt/codex/")
            || normalized.starts_with("/usr/lib/codex/")
            || normalized.contains("/app/codex/")
            || normalized.ends_with("/codex-desktop"));

    macos_app || windows_app || linux_app
}

#[cfg(target_os = "windows")]
mod platform {
    use super::{CLOSE_TIMEOUT, POLL_INTERVAL, is_codex_desktop_process};
    use std::error::Error;
    use std::ffi::c_void;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
    };

    #[derive(Clone, Copy)]
    struct Process {
        pid: u32,
    }

    struct OwnedHandle(HANDLE);

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub(super) fn close_if_running() -> Result<bool, Box<dyn Error>> {
        let processes = find_codex_processes()?;
        if processes.is_empty() {
            return Ok(false);
        }

        close_windows(&processes)?;
        if wait_until_closed(CLOSE_TIMEOUT)? {
            return Ok(true);
        }

        let mut force_close_errors = Vec::new();
        for process in find_codex_processes()? {
            match Command::new("taskkill")
                .args(["/PID", &process.pid.to_string(), "/T", "/F"])
                .stdout(Stdio::null())
                .stderr(Stdio::piped())
                .output()
            {
                Ok(output) if output.status.success() => {}
                Ok(output) => force_close_errors.push(format!(
                    "进程 {}：{}",
                    process.pid,
                    String::from_utf8_lossy(&output.stderr).trim()
                )),
                Err(error) => {
                    force_close_errors.push(format!("进程 {}：{error}", process.pid));
                }
            }
        }
        if !wait_until_closed(CLOSE_TIMEOUT)? {
            let pids = find_codex_processes()?
                .iter()
                .map(|process| process.pid.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let details = if force_close_errors.is_empty() {
                String::new()
            } else {
                format!("。系统错误：{}", force_close_errors.join("；"))
            };
            return Err(format!("Codex 仍在运行，进程号：{pids}{details}").into());
        }
        Ok(true)
    }

    fn find_codex_processes() -> Result<Vec<Process>, Box<dyn Error>> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(format!(
                "读取 Windows 进程列表失败：{}",
                std::io::Error::last_os_error()
            )
            .into());
        }
        let snapshot = OwnedHandle(snapshot);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = Vec::new();
        let mut has_entry = unsafe { Process32FirstW(snapshot.0, &mut entry) } != 0;
        while has_entry {
            let end = entry
                .szExeFile
                .iter()
                .position(|character| *character == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
            if (name.eq_ignore_ascii_case("Codex.exe") || name.eq_ignore_ascii_case("ChatGPT.exe"))
                && let Some(executable) = process_image_path(entry.th32ProcessID)
                && is_codex_desktop_process(&name, &executable)
            {
                found.push(Process {
                    pid: entry.th32ProcessID,
                });
            }
            has_entry = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
        }
        Ok(found)
    }

    fn process_image_path(pid: u32) -> Option<PathBuf> {
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let handle = OwnedHandle(handle);
        let mut buffer = vec![0u16; 32_768];
        let mut length = buffer.len() as u32;
        if unsafe { QueryFullProcessImageNameW(handle.0, 0, buffer.as_mut_ptr(), &mut length) } == 0
        {
            return None;
        }
        Some(PathBuf::from(String::from_utf16_lossy(
            &buffer[..length as usize],
        )))
    }

    struct WindowTargets<'a> {
        pids: &'a [Process],
    }

    unsafe extern "system" fn close_window(hwnd: HWND, parameter: LPARAM) -> i32 {
        let targets = unsafe { &*(parameter as *const WindowTargets<'_>) };
        let mut pid = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, &mut pid);
        }
        if targets.pids.iter().any(|process| process.pid == pid) {
            unsafe {
                PostMessageW(hwnd, WM_CLOSE, 0, 0);
            }
        }
        1
    }

    fn close_windows(processes: &[Process]) -> Result<(), Box<dyn Error>> {
        let targets = WindowTargets { pids: processes };
        let result = unsafe {
            EnumWindows(
                Some(close_window),
                (&targets as *const WindowTargets<'_>).cast::<c_void>() as LPARAM,
            )
        };
        if result == 0 {
            return Err(format!(
                "请求 Codex 正常关闭失败：{}",
                std::io::Error::last_os_error()
            )
            .into());
        }
        Ok(())
    }

    fn wait_until_closed(timeout: std::time::Duration) -> Result<bool, Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            if find_codex_processes()?.is_empty() {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{CLOSE_TIMEOUT, POLL_INTERVAL, is_codex_desktop_process};
    use std::error::Error;
    use std::path::Path;
    use std::process::Command;
    use std::thread;
    use std::time::Instant;

    pub(super) fn close_if_running() -> Result<bool, Box<dyn Error>> {
        let pids = find_codex_processes()?;
        if pids.is_empty() {
            return Ok(false);
        }

        for application in ["Codex", "ChatGPT"] {
            let script = format!("tell application \"{application}\" to quit");
            let _ = Command::new("osascript").args(["-e", &script]).status();
        }
        if wait_until_closed(CLOSE_TIMEOUT)? {
            return Ok(true);
        }

        signal_processes("-TERM")?;
        if wait_until_closed(CLOSE_TIMEOUT)? {
            return Ok(true);
        }
        signal_processes("-KILL")?;
        if !wait_until_closed(CLOSE_TIMEOUT)? {
            return Err("Codex 仍在运行".into());
        }
        Ok(true)
    }

    fn find_codex_processes() -> Result<Vec<u32>, Box<dyn Error>> {
        let mut found = Vec::new();
        for name in ["Codex", "ChatGPT"] {
            let output = Command::new("pgrep").args(["-x", name]).output()?;
            if output.status.code() == Some(1) {
                continue;
            }
            if !output.status.success() {
                return Err(format!("读取 macOS 进程列表失败：{}", output.status).into());
            }
            for line in String::from_utf8(output.stdout)?.lines() {
                let pid = line.trim().parse::<u32>()?;
                let command = Command::new("ps")
                    .args(["-p", &pid.to_string(), "-o", "command="])
                    .output()?;
                let executable = String::from_utf8(command.stdout)?;
                if command.status.success()
                    && is_codex_desktop_process(name, Path::new(executable.trim()))
                {
                    found.push(pid);
                }
            }
        }
        Ok(found)
    }

    fn signal_processes(signal: &str) -> Result<(), Box<dyn Error>> {
        for pid in find_codex_processes()? {
            let _status = Command::new("kill")
                .args([signal, &pid.to_string()])
                .status()?;
        }
        Ok(())
    }

    fn wait_until_closed(timeout: std::time::Duration) -> Result<bool, Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            if find_codex_processes()?.is_empty() {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    use super::{CLOSE_TIMEOUT, POLL_INTERVAL, is_codex_desktop_process};
    use std::error::Error;
    use std::fs;
    use std::process::Command;
    use std::thread;
    use std::time::Instant;

    pub(super) fn close_if_running() -> Result<bool, Box<dyn Error>> {
        if find_codex_processes()?.is_empty() {
            return Ok(false);
        }
        signal_processes("-TERM")?;
        if wait_until_closed(CLOSE_TIMEOUT)? {
            return Ok(true);
        }
        signal_processes("-KILL")?;
        if !wait_until_closed(CLOSE_TIMEOUT)? {
            return Err("Codex 仍在运行".into());
        }
        Ok(true)
    }

    fn find_codex_processes() -> Result<Vec<u32>, Box<dyn Error>> {
        let mut found = Vec::new();
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|value| value.parse::<u32>().ok())
            else {
                continue;
            };
            let process_dir = entry.path();
            let Ok(name) = fs::read_to_string(process_dir.join("comm")) else {
                continue;
            };
            let Ok(executable) = fs::read_link(process_dir.join("exe")) else {
                continue;
            };
            if is_codex_desktop_process(name.trim(), &executable) {
                found.push(pid);
            }
        }
        Ok(found)
    }

    fn signal_processes(signal: &str) -> Result<(), Box<dyn Error>> {
        for pid in find_codex_processes()? {
            let _status = Command::new("kill")
                .args([signal, &pid.to_string()])
                .status()?;
        }
        Ok(())
    }

    fn wait_until_closed(timeout: std::time::Duration) -> Result<bool, Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            if find_codex_processes()?.is_empty() {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn recognizes_only_desktop_installations() {
        assert!(is_codex_desktop_process(
            "Codex",
            Path::new("/Applications/Codex.app/Contents/MacOS/Codex")
        ));
        assert!(is_codex_desktop_process(
            "ChatGPT",
            Path::new("/Applications/ChatGPT.app/Contents/MacOS/ChatGPT")
        ));
        assert!(is_codex_desktop_process(
            "Codex.exe",
            Path::new(r"C:\Users\fixture\AppData\Local\Programs\Codex\Codex.exe")
        ));
        assert!(is_codex_desktop_process(
            "ChatGPT.exe",
            Path::new(
                r"C:\Program Files\WindowsApps\OpenAI.Codex_26.908.4834.0_x64__fixture\app\ChatGPT.exe"
            )
        ));
        assert!(is_codex_desktop_process(
            "codex.exe",
            Path::new(r"C:\Users\fixture\AppData\Local\OpenAI\Codex\bin\build\codex.exe")
        ));
        assert!(is_codex_desktop_process(
            "codex-desktop",
            Path::new("/opt/codex/codex-desktop")
        ));
        assert!(!is_codex_desktop_process(
            "codex.exe",
            Path::new(r"C:\Users\fixture\.codex\bin\codex.exe")
        ));
        assert!(!is_codex_desktop_process(
            "QuotaPlusPlus.exe",
            Path::new(r"C:\Program Files\Codex\QuotaPlusPlus.exe")
        ));
        assert!(!is_codex_desktop_process(
            "ChatGPT.exe",
            Path::new(
                r"C:\Program Files\WindowsApps\OpenAI.ChatGPT-Desktop_1.0.0.0_x64__fixture\app\ChatGPT.exe"
            )
        ));
        assert!(!is_codex_desktop_process(
            "CSwitch",
            Path::new("/Applications/Codex.app/Contents/MacOS/CSwitch")
        ));
        assert!(!is_codex_desktop_process(
            "ChatGPT",
            Path::new("/Applications/ChatGPT Classic.app/Contents/MacOS/ChatGPT")
        ));
    }

    #[test]
    fn a_close_failure_prevents_the_following_write() {
        let wrote = Cell::new(false);
        let result = after_closed_with(
            || Err::<bool, Box<dyn Error>>("关闭失败".into()),
            || {
                wrote.set(true);
                Ok(())
            },
        );
        assert!(result.is_err());
        assert!(!wrote.get());
    }

    #[test]
    fn no_running_process_continues_to_the_write() {
        let wrote = Cell::new(false);
        after_closed_with(
            || Ok(false),
            || {
                wrote.set(true);
                Ok(())
            },
        )
        .expect("continue after process check");
        assert!(wrote.get());
    }
}
