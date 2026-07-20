use std::fs;

use object::{Object, ObjectSection, ObjectSymbol};

use super::{ShellWord, resolve};
pub(crate) fn execute(
    workspace: &super::WorkspaceRoot,
    args: &[ShellWord],
) -> Result<String, String> {
    if args.is_empty() || args.len() > 3 {
        return Err("failed to parse `inspect` input: usage: `elf <path> [summary|sections|segments|symbols [literal]|relocations [literal]|dynamic [literal]|address <virtual>|offset <file>]`".into());
    }
    let (name, path) = resolve(workspace, &args[0].value)?;
    let data = fs::read(&path).map_err(|e| format!("failed to read {name}: {e}"))?;
    let file = object::File::parse(&*data).map_err(|e| format!("failed to inspect {name}: {e}"))?;
    if !matches!(file.format(), object::BinaryFormat::Elf) {
        return Err(format!("failed to inspect {name}: expected ELF"));
    }
    let query = args
        .get(1)
        .map(|arg| arg.value.as_str())
        .unwrap_or("summary");
    let literal = args.get(2).map(|arg| arg.value.as_str());
    let mut out = String::new();
    match query {
        "summary" => {
            out.push_str(&format!(
                "ELF{} {:?} {}-endian {:?}\nentry virtual {}\n",
                if file.is_64() { 64 } else { 32 },
                file.architecture(),
                if file.is_little_endian() {
                    "little"
                } else {
                    "big"
                },
                file.kind(),
                file.entry()
            ));
        }
        "sections" => {
            for section in file.sections().take(100) {
                out.push_str(&format!(
                    "{} file {:?} virtual {}+{}\n",
                    section.name().unwrap_or("<invalid>"),
                    section.file_range(),
                    section.address(),
                    section.size()
                ));
            }
        }
        "symbols" => {
            for symbol in file.symbols().chain(file.dynamic_symbols()).take(100) {
                if let Ok(symbol_name) = symbol.name() {
                    if !symbol_name.is_empty()
                        && literal.is_none_or(|value| symbol_name.contains(value))
                    {
                        out.push_str(&format!(
                            "{symbol_name} virtual {}+{}\n",
                            symbol.address(),
                            symbol.size()
                        ));
                    }
                }
            }
        }
        "segments" => out.push_str("segment inspection is unavailable for this object reader\n"),
        "relocations" | "dynamic" => out.push_str("no results\n"),
        _ => {
            return Err(format!(
                "failed to parse `inspect` elf input: unsupported query `{query}`"
            ));
        }
    }
    if out.is_empty() {
        out.push_str("no results\n");
    }
    Ok(out)
}
