//! The one system call std does not wrap.

/// Set the process's file-mode creation mask to 077: every file and directory
/// created afterwards, by this daemon or by tailscale-rs, is private to its owner.
pub fn private_umask() {
    // SAFETY: umask(2) only replaces the calling process's creation mask. It
    // takes an integer, reads and writes no memory of ours and cannot fail.
    unsafe {
        libc::umask(0o077);
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn files_created_after_the_mask_are_private() {
        super::private_umask();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/tsrsd/test").join(format!("umask-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f"), b"x").unwrap();
        assert_eq!(std::fs::metadata(dir.join("f")).unwrap().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
