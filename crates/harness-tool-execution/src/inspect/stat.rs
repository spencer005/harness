use std::{fs, os::unix::fs::MetadataExt};

use harness_tool_api::{
    InspectJobSuccess, InspectPathKind, InspectStatEntry, InspectStatRequest, InspectStatResult,
    InspectUnixMetadata,
};

use super::{InspectCommandOutput, ShellWord, resolve};

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

pub(crate) fn prepare(args: &[ShellWord]) -> Result<InspectStatRequest, String> {
    let mut metadata = false;
    let mut paths = Vec::new();
    for arg in args {
        match arg.value.as_str() {
            "--metadata" if !metadata => metadata = true,
            "--metadata" => {
                return Err(
                    "failed to parse `inspect` stat input: duplicate `--metadata`".into(),
                );
            }
            value if value.starts_with('-') => {
                return Err(format!(
                    "failed to parse `inspect` stat input: unsupported option `{value}`"
                ));
            }
            value => paths.push(value.to_owned()),
        }
    }
    if paths.is_empty() {
        return Err(
            "failed to parse `inspect` input: usage: `stat <path> [path ...] [--metadata]`".into(),
        );
    }
    Ok(InspectStatRequest { paths, metadata })
}

pub(crate) fn execute(
    workspace: &super::WorkspaceRoot,
    request: &InspectStatRequest,
) -> Result<InspectCommandOutput, String> {
    let mut model = String::new();
    let mut entries = Vec::new();
    for (index, requested_path) in request.paths.iter().enumerate() {
        let (name, path) = resolve(workspace, requested_path)?;
        if index > 0 {
            model.push('\n');
        }
        let value =
            fs::symlink_metadata(&path).map_err(|e| format!("failed to stat {name}: {e}"))?;
        model.push_str(&format!(
            "{name}\nsize: {} bytes\nmodified: {}\npermissions: {:04o}\n",
            value.len(),
            format_timestamp(value.mtime()),
            value.mode() & 0o7777
        ));
        let extended = request.metadata.then(|| InspectUnixMetadata {
            uid: value.uid(),
            gid: value.gid(),
            inode: value.ino(),
            device: value.dev(),
            links: value.nlink(),
            blocks: value.blocks(),
        });
        if let Some(metadata) = &extended {
            model.push_str(&format!(
                "uid: {}\ngid: {}\ninode: {}\ndevice: {}\nlinks: {}\nblocks: {}\n",
                metadata.uid,
                metadata.gid,
                metadata.inode,
                metadata.device,
                metadata.links,
                metadata.blocks
            ));
        }
        let kind = if value.is_dir() {
            InspectPathKind::Directory
        } else if value.file_type().is_symlink() {
            InspectPathKind::Symlink
        } else if value.is_file() {
            InspectPathKind::File
        } else {
            InspectPathKind::Other
        };
        entries.push(InspectStatEntry {
            path: name,
            kind,
            byte_count: value.len(),
            modified_unix_seconds: value.mtime(),
            permissions: value.mode() & 0o7777,
            metadata: extended,
        });
    }
    Ok(InspectCommandOutput {
        model,
        result: InspectJobSuccess::Stat(InspectStatResult { entries }),
    })
}
