use std::{
    fs,
    io::{Read, Seek, SeekFrom},
};

use harness_tool_api::{
    InspectByteSearchRequest, InspectByteSearchResult, InspectBytesRequest, InspectBytesResult,
    InspectJobSuccess,
};

use super::{InspectCommandOutput, ShellWord, resolve};

pub(crate) fn prepare(args: &[ShellWord]) -> Result<InspectBytesRequest, String> {
    if args.len() != 2 {
        return Err(
            "failed to parse `inspect` input: usage: `bytes <path> <offset>+<length>`".into(),
        );
    }
    let (offset, length) = range(&args[1].value)?;
    Ok(InspectBytesRequest {
        path: args[0].value.clone(),
        offset,
        length,
    })
}

pub(crate) fn execute(
    workspace: &super::WorkspaceRoot,
    request: &InspectBytesRequest,
) -> Result<InspectCommandOutput, String> {
    let (name, path) = resolve(workspace, &request.path)?;
    let mut file = fs::File::open(&path).map_err(|e| format!("failed to read {path:?}: {e}"))?;
    let size = file
        .metadata()
        .map_err(|e| format!("failed to inspect {name}: {e}"))?
        .len();
    if request.offset > size {
        return Err(format!(
            "failed to read {name}: offset {} is beyond file size {size}",
            request.offset
        ));
    }
    file.seek(SeekFrom::Start(request.offset))
        .map_err(|e| format!("failed to seek {name}: {e}"))?;
    let actual = request.length.min((size - request.offset) as usize);
    let mut data = vec![0; actual];
    file.read_exact(&mut data)
        .map_err(|e| format!("failed to read {name}: {e}"))?;
    let mut model = format!(
        "{name} {} bytes\nrange: {}+{actual}\n{}\n",
        size,
        request.offset,
        hex(&data)
    );
    let next_offset = (request.offset + (actual as u64) < size)
        .then_some(request.offset + actual as u64);
    if let Some(next) = next_offset {
        model.push_str(&format!("next: {next}+{}\n", request.length));
    }
    Ok(InspectCommandOutput {
        model,
        result: InspectJobSuccess::Bytes(InspectBytesResult {
            path: name,
            file_size: size,
            offset: request.offset,
            bytes: data,
            next_offset,
        }),
    })
}

pub(crate) fn prepare_search(args: &[ShellWord]) -> Result<InspectByteSearchRequest, String> {
    if args.len() != 2 {
        return Err("failed to parse `inspect` input: usage: `byte-search <path> <hex>`".into());
    }
    Ok(InspectByteSearchRequest {
        path: args[0].value.clone(),
        pattern: decode(&args[1].value)?,
    })
}

pub(crate) fn execute_search(
    workspace: &super::WorkspaceRoot,
    request: &InspectByteSearchRequest,
) -> Result<InspectCommandOutput, String> {
    let (name, path) = resolve(workspace, &request.path)?;
    let data = fs::read(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let mut offsets = Vec::new();
    let mut total_matches = 0;
    if request.pattern.len() <= data.len() {
        for start in 0..=data.len() - request.pattern.len() {
            if data[start..start + request.pattern.len()] == request.pattern {
                if offsets.len() < 100 {
                    offsets.push(start as u64);
                }
                total_matches += 1;
            }
        }
    }
    let mut model = String::new();
    for offset in &offsets {
        model.push_str(&format!("{offset}\n"));
    }
    if total_matches == 0 {
        model.push_str("no results\n");
    } else if total_matches > offsets.len() {
        model.push_str(&format!(
            "[byte-search output truncated: showing first 100 of {total_matches} offsets]\n"
        ));
    }
    Ok(InspectCommandOutput {
        model,
        result: InspectJobSuccess::ByteSearch(InspectByteSearchResult {
            path: name,
            offsets,
            total_matches,
        }),
    })
}

fn range(value: &str) -> Result<(u64, usize), String> {
    let (offset, length) = value
        .split_once('+')
        .ok_or("range must be `offset+length`")?;
    let offset = offset
        .parse()
        .map_err(|_| "offset must be a non-negative integer")?;
    let length = length
        .parse()
        .map_err(|_| "length must be a positive integer")?;
    if length == 0 {
        return Err("length must be positive".to_string());
    }
    Ok((offset, length))
}

fn decode(value: &str) -> Result<Vec<u8>, String> {
    if value.is_empty() || value.len() % 2 != 0 {
        return Err("hex sequence must contain a non-empty even number of digits".into());
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let hi = digit(pair[0]).ok_or("hex sequence contains a non-hexadecimal digit")?;
            let lo = digit(pair[1]).ok_or("hex sequence contains a non-hexadecimal digit")?;
            Ok(hi << 4 | lo)
        })
        .collect()
}

fn digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

fn hex(data: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = vec![0u8; data.len() * 2];
    for (i, byte) in data.iter().enumerate() {
        out[i * 2] = DIGITS[(byte >> 4) as usize];
        out[i * 2 + 1] = DIGITS[(byte & 0xf) as usize];
    }
    unsafe { String::from_utf8_unchecked(out) }
}
