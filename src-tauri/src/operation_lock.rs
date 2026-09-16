use fs2::FileExt;
use std::error::Error;
use std::fs::{File, OpenOptions};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

#[derive(Debug)]
pub struct OperationLock {
    _files: Vec<File>,
}

const LOCK_FILES: [&str; 2] = ["cswitch-operation.lock", "qpp-operation.lock"];

pub fn acquire(codex_home: &Path) -> Result<OperationLock, Box<dyn Error>> {
    std::fs::create_dir_all(codex_home).map_err(|error| -> Box<dyn Error> {
        format!("创建 Codex 目录失败 {}：{error}", codex_home.display()).into()
    })?;
    let mut files = Vec::with_capacity(LOCK_FILES.len());
    for name in LOCK_FILES {
        files.push(acquire_file(&codex_home.join(name))?);
    }
    Ok(OperationLock { _files: files })
}

fn acquire_file(path: &Path) -> Result<File, Box<dyn Error>> {
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let file = options.open(path).map_err(|error| -> Box<dyn Error> {
        format!("打开操作锁文件失败 {}：{error}", path.display()).into()
    })?;
    file.try_lock_exclusive()
        .map_err(|error| -> Box<dyn Error> {
            format!("另一个 CSwitch 实例正在操作，请等待其完成后重试：{error}").into()
        })?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn rejects_a_second_process_level_operation() {
        let directory = tempdir().expect("tempdir");
        let first = acquire(directory.path()).expect("first lock");
        let error = acquire(directory.path()).expect_err("second lock should fail");
        assert!(error.to_string().contains("另一个 CSwitch"));
        drop(first);
        acquire(directory.path()).expect("lock after release");
    }

    #[test]
    fn rejects_an_operation_while_the_legacy_lock_is_held() {
        let directory = tempdir().expect("tempdir");
        let legacy = acquire_file(&directory.path().join("qpp-operation.lock"))
            .expect("legacy operation lock");

        let error = acquire(directory.path()).expect_err("legacy lock should block CSwitch");

        assert!(error.to_string().contains("CSwitch"), "{error}");
        drop(legacy);
        acquire(directory.path()).expect("lock after legacy release");
    }
}
