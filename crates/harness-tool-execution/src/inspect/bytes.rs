use std::{
    fs,
    io::{Read, Seek, SeekFrom},
};

use super::{ShellWord, resolve};

pub(crate) fn execute(
    workspace: &super::WorkspaceRoot,
    args: &[ShellWord],
) -> Result<String, String> {
    if args.len() != 2 {
        return Err(
            "failed to parse `inspect` input: usage: `bytes <path> <offset>+<length>`".into(),
        );
    }
    let (offset, length) = range(&args[1].value)?;
    let (name, path) = resolve(workspace, &args[0].value)?;
    let mut file = fs::File::open(&path).map_err(|e| format!("failed to read {path:?}: {e}"))?;
    let size = file
        .metadata()
        .map_err(|e| format!("failed to inspect {name}: {e}"))?
        .len();
    if offset > size {
        return Err(format!(
            "failed to read {name}: offset {offset} is beyond file size {size}"
        ));
    }
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("failed to seek {name}: {e}"))?;
    let actual = length.min((size - offset) as usize);
    let mut data = vec![0; actual];
    file.read_exact(&mut data)
        .map_err(|e| format!("failed to read {name}: {e}"))?;
    let mut output = format!(
        "{name} {} bytes\nrange: {offset}+{actual}\n{}\n",
        size,
        hex(&data)
    );
    if offset + (actual as u64) < size {
        output.push_str(&format!("next: {}+{length}\n", offset + actual as u64));
    }
    Ok(output)
}

pub(crate) fn search(
    workspace: &super::WorkspaceRoot,
    args: &[ShellWord],
) -> Result<String, String> {
    if args.len() != 2 {
        return Err("failed to parse `inspect` input: usage: `byte-search <path> <hex>`".into());
    }
    let pattern = decode(&args[1].value)?;
    let (_, path) = resolve(workspace, &args[0].value)?;
    let data = fs::read(&path).map_err(|e| format!("failed to read {}: {e}", path.display()))?;
    let mut output = String::new();
    let mut count = 0;
    if pattern.len() <= data.len() {
        for start in 0..=data.len() - pattern.len() {
            if data[start..start + pattern.len()] == pattern {
                if count < 100 {
                    output.push_str(&format!("{start}\n"));
                }
                count += 1;
            }
        }
    }
    if count == 0 {
        output.push_str("no results\n");
    } else if count > 100 {
        output.push_str(&format!(
            "[byte-search output truncated: showing first 100 of {count} offsets]\n"
        ));
    }
    Ok(output)
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
