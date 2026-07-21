use std::{fs, os::unix::fs::MetadataExt};

use libc;

use super::{ShellWord, resolve};

fn format_timestamp(secs: i64) -> String {
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ts: libc::time_t = secs as libc::time_t;
    unsafe {
        libc::localtime_r(&ts, &mut tm);
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

pub(crate) fn execute(
    workspace: &super::WorkspaceRoot,
    args: &[ShellWord],
) -> Result<String, String> {
    if args.is_empty() {
        return Err("failed to parse `inspect` input: usage: `stat <path> [path ...] [--metadata]`".into());
    }
    let metadata = args.iter().any(|arg| arg.value == "--metadata");
    let paths = args
        .iter()
        .filter(|arg| !arg.value.starts_with('-'))
        .collect::<Vec<_>>();
    let mut out = String::new();
    for (index, arg) in paths.iter().enumerate() {
        let (name, path) = resolve(workspace, &arg.value)?;
        if index > 0 {
            out.push('\n');
        }
        let value =
            fs::symlink_metadata(&path).map_err(|e| format!("failed to stat {name}: {e}"))?;
        out.push_str(&format!(
            "{name}\nsize: {} bytes\nmodified: {}\npermissions: {:04o}\n",
            value.len(),
            format_timestamp(value.mtime()),
            value.mode() & 0o7777
        ));
        if metadata {
            out.push_str(&format!(
                "uid: {}\ngid: {}\ninode: {}\ndevice: {}\nlinks: {}\nblocks: {}\n",
                value.uid(),
                value.gid(),
                value.ino(),
                value.dev(),
                value.nlink(),
                value.blocks()
            ));
        }
    }
    Ok(out)
}
