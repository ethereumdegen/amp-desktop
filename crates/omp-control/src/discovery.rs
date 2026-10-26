use anyhow::{bail, Context, Result};
use std::{
    env,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command, time::timeout};

#[derive(Clone, Debug)]
pub struct Installation {
    pub path: PathBuf,
    pub version: String,
    pub environment_path: OsString,
}

// Finder does not inherit the user's shell PATH. Do not execute shell startup files:
// those can print, block, or run arbitrary interactive setup.
fn environment_path(executable: Option<&Path>) -> Result<OsString> {
    let mut entries = Vec::<PathBuf>::new();
    let mut add = |path: PathBuf| {
        if !path.as_os_str().is_empty() && !entries.contains(&path) {
            entries.push(path);
        }
    };
    if let Some(parent) = executable.and_then(Path::parent) {
        add(parent.to_owned());
    }
    if let Some(path) = env::var_os("PATH") {
        for path in env::split_paths(&path) {
            add(path);
        }
    }
    if let Some(home) = env::var_os("HOME") {
        add(PathBuf::from(&home).join(".bun/bin"));
        add(PathBuf::from(home).join(".local/bin"));
    }
    if let Some(bun) = env::var_os("BUN_INSTALL") {
        add(PathBuf::from(bun).join("bin"));
    }
    for path in [
        "/opt/homebrew/bin",
        "/usr/local/bin",
        "/usr/bin",
        "/bin",
        "/usr/sbin",
        "/sbin",
    ] {
        add(PathBuf::from(path));
    }
    env::join_paths(entries).context("Cannot construct OMP executable search path")
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn explicit_path(value: &str, search: &OsString) -> Result<PathBuf> {
    if value.trim().is_empty() {
        bail!("OMP executable path is empty");
    }
    let path = if let Some(suffix) = value.strip_prefix("~/") {
        PathBuf::from(env::var_os("HOME").context("HOME is unavailable")?).join(suffix)
    } else {
        PathBuf::from(value)
    };
    if path.components().count() == 1 && !path.is_absolute() {
        for directory in env::split_paths(search) {
            let candidate = directory.join(&path);
            if is_executable(&candidate) {
                return absolute(candidate);
            }
        }
        bail!("Configured OMP executable was not found");
    }
    absolute(path)
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = path.metadata() else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

async fn validate(path: PathBuf) -> Result<Installation> {
    if !is_executable(&path) {
        bail!("OMP path is not an executable file");
    }
    let environment_path = environment_path(Some(&path))?;
    let mut command = Command::new(&path);
    command
        .arg("--version")
        .env("PATH", &environment_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .context("Cannot execute OMP version probe")?;
    let pid = child.id();
    let mut stdout = child
        .stdout
        .take()
        .context("OMP version stdout unavailable")?;
    let probe = timeout(Duration::from_secs(5), async {
        let mut bytes = Vec::new();
        (&mut stdout).take(8193).read_to_end(&mut bytes).await?;
        if bytes.len() > 8192 {
            bail!("OMP version output exceeds limit");
        }
        let status = child.wait().await?;
        if !status.success() {
            bail!("OMP version probe failed");
        }
        let version = String::from_utf8(bytes).context("OMP version is not UTF-8")?;
        let version = version.trim();
        if version.is_empty() || version.len() > 512 || version.chars().any(|c| c.is_control()) {
            bail!("OMP returned an invalid version");
        }
        Ok::<_, anyhow::Error>(version.to_owned())
    })
    .await;
    // Also clean up descendants of a misbehaving version command.
    #[cfg(unix)]
    if let Some(pid) = pid {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    let version = match probe {
        Ok(result) => result?,
        Err(_) => {
            let _ = child.kill().await;
            bail!("OMP version probe timed out");
        }
    };
    Ok(Installation {
        path,
        version,
        environment_path,
    })
}

pub async fn discover(explicit: Option<&str>) -> Result<Installation> {
    let search = environment_path(None)?;
    if let Some(value) = explicit {
        return validate(explicit_path(value, &search)?)
            .await
            .context("Configured OMP installation is unavailable");
    }
    if let Some(value) = env::var_os("OMP_DESKTOP_OMP") {
        let value = value.to_str().context("OMP_DESKTOP_OMP must be UTF-8")?;
        return validate(explicit_path(value, &search)?)
            .await
            .context("OMP_DESKTOP_OMP installation is unavailable");
    }
    let mut candidates = Vec::new();
    for directory in env::split_paths(&search) {
        let path = absolute(directory.join("omp"))?;
        if !candidates.contains(&path) {
            candidates.push(path);
        }
    }
    for path in candidates {
        if is_executable(&path) {
            if let Ok(installation) = validate(path).await {
                return Ok(installation);
            }
        }
    }
    bail!("No working OMP installation found. Install from https://omp.sh/install or configure its executable path.")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finder_path_preserves_executable_and_bun_locations() {
        let path = environment_path(Some(Path::new("/custom/bin/omp"))).unwrap();
        let entries: Vec<_> = env::split_paths(&path).collect();
        assert_eq!(entries[0], PathBuf::from("/custom/bin"));
        assert!(entries.contains(&PathBuf::from("/opt/homebrew/bin")));
        if let Some(home) = env::var_os("HOME") {
            assert!(entries.contains(&PathBuf::from(home).join(".bun/bin")));
        }
    }
    #[tokio::test]
    async fn explicit_invalid_path_does_not_fall_back() {
        assert!(discover(Some("/definitely-not-an-omp-desktop-executable"))
            .await
            .is_err());
    }
}
