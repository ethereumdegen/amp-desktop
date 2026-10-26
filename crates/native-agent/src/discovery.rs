//! Descriptor-relative directory discovery. Never follows directory symlinks.
use super::*;
use std::ffi::{CStr, OsString};
use std::os::unix::ffi::OsStringExt;

const MAX_DEPTH: usize = 64;
const PRUNED: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".cache",
    ".venv",
    "venv",
    "vendor",
    "coverage",
    "__pycache__",
];

pub(super) struct Entry {
    pub path: PathBuf,
    pub kind: &'static str,
}
pub(super) struct Discovery {
    pub entries: Vec<Entry>,
    pub truncated: bool,
    pub pruned: usize,
    pub visited: usize,
}

pub(super) fn open_directory(root: &Path, target: &Path) -> Result<File> {
    if root == target {
        return Ok(parent_handle(root, &root.join(".directory-handle"), false)?.0);
    }
    let (parent, name) = parent_handle(root, target, false)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("Cannot safely open directory");
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

struct Directory(*mut libc::DIR);
impl Drop for Directory {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0);
        }
    }
}

#[cfg(target_os = "macos")]
unsafe fn errno_pointer() -> *mut libc::c_int {
    libc::__error()
}
#[cfg(not(target_os = "macos"))]
unsafe fn errno_pointer() -> *mut libc::c_int {
    libc::__errno_location()
}

fn children(
    root: &Path,
    path: &Path,
    cancel: &CancellationToken,
    budget: usize,
) -> Result<(Vec<Entry>, bool, usize)> {
    let file = open_directory(root, path)?;
    let duplicate = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error()).context("Cannot duplicate directory handle");
    }
    let raw = unsafe { libc::fdopendir(duplicate) };
    if raw.is_null() {
        unsafe {
            libc::close(duplicate);
        }
        return Err(std::io::Error::last_os_error()).context("Cannot enumerate directory");
    }
    let directory = Directory(raw);
    // Sort complete directories. If a scan overflows, omit its arbitrary
    // filesystem-order subset entirely rather than report nondeterministic hits.
    let mut entries = Vec::new();
    let mut truncated = false;
    loop {
        check_cancel(cancel)?;
        // readdir signals both EOF and failure with null.
        unsafe {
            *errno_pointer() = 0;
        }
        let raw = unsafe { libc::readdir(directory.0) };
        if raw.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error().unwrap_or(0) != 0 {
                return Err(error).context("Cannot finish directory enumeration");
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*raw).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        if entries.len() == budget {
            truncated = true;
            break;
        }
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                file.as_raw_fd(),
                name.as_ptr(),
                &mut stat,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } < 0
        {
            return Err(std::io::Error::last_os_error()).context("Cannot inspect directory entry");
        }
        let kind = match stat.st_mode & libc::S_IFMT {
            libc::S_IFDIR => "directory",
            libc::S_IFREG => "file",
            libc::S_IFLNK => "symlink",
            _ => "other",
        };
        entries.push(Entry {
            path: path.join(OsString::from_vec(name.to_bytes().to_vec())),
            kind,
        });
    }
    let scanned = entries.len();
    if truncated {
        entries.clear();
    } else {
        entries.sort_by(|a, b| a.path.cmp(&b.path));
    }
    Ok((entries, truncated, scanned))
}

pub(super) fn discover(root: &Path, start: &Path, cancel: &CancellationToken) -> Result<Discovery> {
    if open_directory(root, start).is_err() {
        // Validate non-directory targets with the same safe descriptor open.
        open_file(root, start)?;
        return Ok(Discovery {
            entries: vec![Entry {
                path: start.to_owned(),
                kind: "file",
            }],
            truncated: false,
            pruned: 0,
            visited: 1,
        });
    }
    let mut result = Discovery {
        entries: Vec::new(),
        truncated: false,
        pruned: 0,
        visited: 0,
    };
    let mut queue = VecDeque::from([(start.to_owned(), 0)]);
    while let Some((directory, depth)) = queue.pop_front() {
        check_cancel(cancel)?;
        let (entries, truncated, scanned) =
            children(root, &directory, cancel, MAX_ENTRIES - result.visited)?;
        result.visited += scanned;
        result.truncated |= truncated;
        for entry in entries {
            if entry.kind == "directory" {
                if PRUNED
                    .iter()
                    .any(|name| entry.path.file_name().is_some_and(|part| part == *name))
                {
                    result.pruned += 1;
                } else if depth + 1 >= MAX_DEPTH {
                    result.truncated = true;
                } else {
                    queue.push_back((entry.path.clone(), depth + 1));
                }
            }
            result.entries.push(entry);
        }
        if result.visited == MAX_ENTRIES {
            result.truncated |= !queue.is_empty();
            break;
        }
    }
    Ok(result)
}

pub(super) fn read_directory(
    root: &Path,
    target: &Path,
    args: &Value,
    cancel: &CancellationToken,
) -> Result<String> {
    let offset = integer(args, "offset", 1, u64::MAX)?;
    let limit = integer(args, "limit", 200, 2000)? as usize;
    let (entries, scan_truncated, scanned) = children(root, target, cancel, MAX_ENTRIES)?;
    let mut output = Vec::new();
    let mut bytes = 0;
    let mut truncated = scan_truncated;
    for (index, entry) in entries.iter().enumerate() {
        if (index as u64) < offset - 1 {
            continue;
        }
        let value = json!({"name":entry.path.file_name().unwrap_or_default().to_string_lossy(), "type":entry.kind});
        let size = value.to_string().len() + 1;
        if output.len() == limit || bytes + size > MAX_OUTPUT_BYTES - 1024 {
            truncated = true;
            break;
        }
        bytes += size;
        output.push(value);
    }
    bounded_json(
        json!({"path":target.strip_prefix(root)?.to_string_lossy(),"entries":output,"offset":offset,"truncated":truncated,"scannedEntries":scanned,"scanTruncated":scan_truncated,
        "scanLimitNote":if scan_truncated { "Directory scan limit exceeded; arbitrary partial entries omitted. Scope a known child path." } else { "" }}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oversized_scan_is_bounded_and_never_returns_an_arbitrary_subset() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs::canonicalize(directory.path()).unwrap();
        for name in ["c", "a", "b"] {
            fs::write(root.join(name), "").unwrap();
        }
        let cancel = CancellationToken::new();
        let (entries, truncated, scanned) = children(&root, &root, &cancel, 2).unwrap();
        assert!(entries.is_empty());
        assert!(truncated);
        assert_eq!(scanned, 2);
        let (entries, truncated, scanned) = children(&root, &root, &cancel, 3).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.path.file_name().unwrap().to_str().unwrap())
                .collect::<Vec<_>>(),
            ["a", "b", "c"]
        );
        assert!(!truncated);
        assert_eq!(scanned, 3);
        let listing: Value = serde_json::from_str(
            &read_directory(&root, &root, &json!({"offset":2,"limit":1}), &cancel).unwrap(),
        )
        .unwrap();
        assert_eq!(listing["entries"][0]["name"], "b");
        assert_eq!(listing["truncated"], true);
    }
}
