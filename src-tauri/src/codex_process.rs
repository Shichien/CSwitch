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
    crate::oauth::ensure_login_active()?;
    let closed = platform::close_if_running()?;
    crate::oauth::ensure_login_active()?;
    assert_no_other_writers()?;
    Ok(closed)
}

fn assert_no_other_writers() -> Result<(), Box<dyn Error>> {
    let mut system = sysinfo::System::new();
    system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let mut writers = Vec::new();
    for process in system.processes().values() {
        let name = process.name().to_string_lossy().to_ascii_lowercase();
        if !matches!(name.as_str(), "codex" | "codex.exe" | "codex-desktop") {
            continue;
        }
        let Some(path) = process.exe() else {
            return Err(format!(
                "检测到 Codex 进程 {}，但系统拒绝读取路径，请关闭该进程后重试",
                process.pid()
            )
            .into());
        };
        if is_codex_desktop_process(&name, path) {
            return Err(format!(
                "Codex 在关闭后重新启动，进程 {}：{}",
                process.pid(),
                path.display()
            )
            .into());
        }
        // A CLI/IDE can use the same history. Do not silently mutate underneath it,
        // and do not kill an unverified terminal session merely because of its name.
        writers.push(format!("进程 {}：{}", process.pid(), path.display()));
    }
    if writers.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "仍有 Codex CLI、IDE 或后台服务运行，请先关闭后重试：\n{}",
            writers.join("\n")
        )
        .into())
    }
}

fn is_codex_desktop_process(name: &str, executable: &Path) -> bool {
    let normalized = executable
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();
    let lower_name = name.to_lowercase();

    let macos_app = (lower_name == "codex" && normalized.ends_with(".app/contents/macos/codex"))
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
    let bundled_cli = executable
        .parent()
        .filter(|parent| parent.file_name().is_some_and(|name| name == "MacOS"))
        .and_then(Path::parent)
        .is_some_and(|contents| contents.join("Resources/codex").is_file());
    let linux_app = (lower_name == "codex" || lower_name == "codex-desktop")
        && (normalized.starts_with("/opt/codex/")
            || normalized.starts_with("/usr/lib/codex/")
            || normalized.contains("/app/codex/")
            || normalized.contains("/.mount_")
            || normalized.starts_with("/snap/codex/")
            || normalized.ends_with("/codex-desktop"));

    macos_app || bundled_cli || windows_app || linux_app
}

#[cfg(target_os = "windows")]
mod platform {
    use super::{CLOSE_TIMEOUT, POLL_INTERVAL, is_codex_desktop_process};
    use std::error::Error;
    use std::ffi::c_void;
    use std::path::PathBuf;
    use std::thread;
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{
        CloseHandle, FILETIME, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, WAIT_OBJECT_0,
        WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
        PROCESS_TERMINATE, QueryFullProcessImageNameW, TerminateProcess, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
    };

    struct Process {
        pid: u32,
        executable: PathBuf,
        created: u64,
        handle: OwnedHandle,
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
        if wait_until_closed(&processes, CLOSE_TIMEOUT)? {
            return Ok(true);
        }

        let mut force_close_errors = Vec::new();
        for process in &processes {
            crate::oauth::ensure_login_active()?;
            if let Err(error) = terminate_verified_process(process) {
                force_close_errors.push(error.to_string());
            }
        }
        if !wait_until_closed(&processes, CLOSE_TIMEOUT)? {
            let pids = processes
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
                && let Some(process) = process_identity(entry.th32ProcessID)?
                && is_codex_desktop_process(&name, &process.executable)
            {
                found.push(process);
            }
            has_entry = unsafe { Process32NextW(snapshot.0, &mut entry) } != 0;
        }
        Ok(found)
    }

    fn process_identity(pid: u32) -> Result<Option<Process>, Box<dyn Error>> {
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(87) {
                return Ok(None);
            }
            return Err(format!("读取 Codex 进程 {pid} 失败：{error}").into());
        }
        let handle = OwnedHandle(handle);
        if has_exited(&handle)? {
            return Ok(None);
        }
        match handle_identity(&handle, pid) {
            Ok((executable, created)) => Ok(Some(Process {
                pid,
                handle,
                executable,
                created,
            })),
            Err(_) if has_exited(&handle)? => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn has_exited(handle: &OwnedHandle) -> Result<bool, Box<dyn Error>> {
        match unsafe { WaitForSingleObject(handle.0, 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(format!(
                "检查 Codex 退出状态失败：{}",
                std::io::Error::last_os_error()
            )
            .into()),
        }
    }

    fn handle_identity(handle: &OwnedHandle, pid: u32) -> Result<(PathBuf, u64), Box<dyn Error>> {
        let mut buffer = vec![0u16; 32_768];
        let mut length = buffer.len() as u32;
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        if unsafe { QueryFullProcessImageNameW(handle.0, 0, buffer.as_mut_ptr(), &mut length) } == 0
            || unsafe {
                GetProcessTimes(handle.0, &mut created, &mut exited, &mut kernel, &mut user)
            } == 0
        {
            return Err(format!(
                "查询 Codex 进程 {pid} 身份失败：{}",
                std::io::Error::last_os_error()
            )
            .into());
        }
        Ok((
            PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize])),
            ((created.dwHighDateTime as u64) << 32) | created.dwLowDateTime as u64,
        ))
    }

    fn terminate_verified_process(process: &Process) -> Result<(), Box<dyn Error>> {
        if has_exited(&process.handle)? {
            return Ok(());
        }
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | PROCESS_SYNCHRONIZE,
                0,
                process.pid,
            )
        };
        if handle.is_null() {
            let error = std::io::Error::last_os_error();
            if has_exited(&process.handle)? {
                return Ok(());
            }
            return Err(format!("打开待关闭 Codex 进程 {} 失败：{error}", process.pid).into());
        }
        let handle = OwnedHandle(handle);
        if unsafe { WaitForSingleObject(handle.0, 0) } == WAIT_OBJECT_0 {
            return Ok(());
        }
        let (path, created) = handle_identity(&handle, process.pid)?;
        if path != process.executable || created != process.created {
            return Err(format!("Codex 进程 {} 已发生变化，保留新进程", process.pid).into());
        }
        // Terminate the verified handle, not a shell command or a subsequently reused PID.
        if unsafe { TerminateProcess(handle.0, 1) } == 0 {
            let error = std::io::Error::last_os_error();
            if has_exited(&process.handle)? {
                return Ok(());
            }
            return Err(format!("系统拒绝关闭 Codex 进程 {}：{}", process.pid, error).into());
        }
        let deadline = Instant::now() + CLOSE_TIMEOUT;
        loop {
            crate::oauth::ensure_login_active()?;
            let status = unsafe { WaitForSingleObject(handle.0, 100) };
            if status == WAIT_OBJECT_0 {
                return Ok(());
            }
            if status != WAIT_TIMEOUT {
                return Err(format!(
                    "等待 Codex 进程 {} 退出失败：{}",
                    process.pid,
                    std::io::Error::last_os_error()
                )
                .into());
            }
            if Instant::now() >= deadline {
                return Err(format!("等待 Codex 进程 {} 退出超时", process.pid).into());
            }
        }
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

    fn wait_until_closed(
        processes: &[Process],
        timeout: std::time::Duration,
    ) -> Result<bool, Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            crate::oauth::ensure_login_active()?;
            // Keep the original kernel handles: do not reopen dying processes by PID on every poll.
            if processes
                .iter()
                .map(|p| has_exited(&p.handle))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .all(|exited| exited)
            {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
    #[cfg(test)]
    mod native_process_tests {
        use super::*;
        #[test]
        fn child_fixture() {
            if std::env::var_os("CSWITCH_NATIVE_CLOSE_FIXTURE").is_some() {
                std::thread::sleep(std::time::Duration::from_secs(20));
            }
        }
        #[test]
        #[ignore = "isolated native child process; run scripts/run-process-tests.py"]
        fn native_close_verifies_identity_and_terminates_without_external_commands() {
            use std::os::windows::process::CommandExt;
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "codex_process::platform::native_process_tests::child_fixture",
                ])
                .env("CSWITCH_NATIVE_CLOSE_FIXTURE", "1")
                .creation_flags(0x08000000)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            let mut process = process_identity(child.id()).unwrap().unwrap();
            let created = process.created;
            process.created += 1;
            assert!(terminate_verified_process(&process).is_err());
            assert!(child.try_wait().unwrap().is_none());
            process.created = created;
            terminate_verified_process(&process).unwrap();
            assert!(!child.wait().unwrap().success());
            assert!(wait_until_closed(std::slice::from_ref(&process), CLOSE_TIMEOUT).unwrap());
            // Child retains its handle after exit. It must not be queried/terminated a second time.
            assert!(process_identity(child.id()).unwrap().is_none());
            terminate_verified_process(&process).unwrap();
        }
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{CLOSE_TIMEOUT, POLL_INTERVAL, is_codex_desktop_process};
    use std::error::Error;
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
        let mut system = sysinfo::System::new();
        system.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let mut found = Vec::new();
        for process in system.processes().values() {
            let name = process.name().to_string_lossy();
            if let Some(executable) = process.exe() {
                if is_codex_desktop_process(&name, executable) {
                    found.push(process.pid().as_u32());
                }
            } else if name.eq_ignore_ascii_case("codex") || name.eq_ignore_ascii_case("chatgpt") {
                return Err(format!(
                    "读取 Codex 桌面进程 {} 路径失败，请检查应用权限",
                    process.pid()
                )
                .into());
            }
        }
        Ok(found)
    }

    fn signal_processes(signal: &str) -> Result<(), Box<dyn Error>> {
        for pid in find_codex_processes()? {
            let output = Command::new("kill")
                .args([signal, &pid.to_string()])
                .output()?;
            if !output.status.success() && find_codex_processes()?.contains(&pid) {
                return Err(format!(
                    "结束 Codex 进程 {pid} 失败：{}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )
                .into());
            }
        }
        Ok(())
    }

    fn wait_until_closed(timeout: std::time::Duration) -> Result<bool, Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            crate::oauth::ensure_login_active()?;
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
            if !matches!(name.trim(), "codex" | "codex-desktop") {
                continue;
            }
            let executable = match fs::read_link(process_dir.join("exe")) {
                Ok(path) => path,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("读取 Codex 进程 {pid} 路径失败：{error}").into()),
            };
            if is_codex_desktop_process(name.trim(), &executable) {
                found.push(pid);
            }
        }
        Ok(found)
    }

    fn signal_processes(signal: &str) -> Result<(), Box<dyn Error>> {
        for pid in find_codex_processes()? {
            let output = Command::new("kill")
                .args([signal, &pid.to_string()])
                .output()?;
            if !output.status.success() && find_codex_processes()?.contains(&pid) {
                return Err(format!(
                    "结束 Codex 进程 {pid} 失败：{}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )
                .into());
            }
        }
        Ok(())
    }

    fn wait_until_closed(timeout: std::time::Duration) -> Result<bool, Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        loop {
            crate::oauth::ensure_login_active()?;
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
            "OtherApplication.exe",
            Path::new(r"C:\Program Files\Codex\OtherApplication.exe")
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
