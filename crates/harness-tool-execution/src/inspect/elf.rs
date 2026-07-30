use std::fs;

use harness_tool_api::{
    InspectElfEntry, InspectElfQuery, InspectElfRequest, InspectElfResult, InspectElfSummary,
    InspectJobSuccess,
};
use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol};

use super::{InspectCommandOutput, ShellWord, resolve};

pub(crate) fn prepare(args: &[ShellWord]) -> Result<InspectElfRequest, String> {
    if args.is_empty() || args.len() > 3 {
        return Err("failed to parse `inspect` input: usage: `elf <path> [summary|sections|segments|symbols [literal]|relocations [literal]|dynamic [literal]|address <virtual>|offset <file>]`".into());
    }
    let query_name = args.get(1).map(|arg| arg.value.as_str()).unwrap_or("summary");
    let argument = args.get(2).map(|arg| arg.value.clone());
    let query = match query_name {
        "summary" if argument.is_none() => InspectElfQuery::Summary,
        "sections" if argument.is_none() => InspectElfQuery::Sections,
        "segments" if argument.is_none() => InspectElfQuery::Segments,
        "symbols" => InspectElfQuery::Symbols(argument),
        "relocations" => InspectElfQuery::Relocations(argument),
        "dynamic" => InspectElfQuery::Dynamic(argument),
        "address" => InspectElfQuery::Address(parse_number(
            argument.as_deref().ok_or(
                "failed to parse `inspect` elf input: `address` requires a virtual address",
            )?,
        )?),
        "offset" => InspectElfQuery::Offset(parse_number(
            argument
                .as_deref()
                .ok_or("failed to parse `inspect` elf input: `offset` requires a file offset")?,
        )?),
        "summary" | "sections" | "segments" => {
            return Err(format!(
                "failed to parse `inspect` elf input: `{query_name}` does not accept an argument"
            ));
        }
        other => {
            return Err(format!(
                "failed to parse `inspect` elf input: unsupported query `{other}`"
            ));
        }
    };
    Ok(InspectElfRequest {
        path: args[0].value.clone(),
        query,
    })
}

pub(crate) fn execute(
    workspace: &super::WorkspaceRoot,
    request: &InspectElfRequest,
) -> Result<InspectCommandOutput, String> {
    let (name, path) = resolve(workspace, &request.path)?;
    let data = fs::read(&path).map_err(|e| format!("failed to read {name}: {e}"))?;
    let file =
        object::File::parse(&*data).map_err(|e| format!("failed to inspect {name}: {e}"))?;
    if !matches!(file.format(), object::BinaryFormat::Elf) {
        return Err(format!("failed to inspect {name}: expected ELF"));
    }

    let mut model = String::new();
    let mut summary = None;
    let mut entries = Vec::new();
    match &request.query {
        InspectElfQuery::Summary => {
            let value = InspectElfSummary {
                bits: if file.is_64() { 64 } else { 32 },
                architecture: format!("{:?}", file.architecture()),
                endianness: if file.is_little_endian() {
                    "little".to_owned()
                } else {
                    "big".to_owned()
                },
                kind: format!("{:?}", file.kind()),
                entry: file.entry(),
            };
            model.push_str(&format!(
                "ELF{} {} {}-endian {}\nentry virtual {}\n",
                value.bits, value.architecture, value.endianness, value.kind, value.entry
            ));
            summary = Some(value);
        }
        InspectElfQuery::Sections => {
            for section in file.sections().take(100) {
                let file_range = section.file_range();
                model.push_str(&format!(
                    "{} file {:?} virtual {}+{}\n",
                    section.name().unwrap_or("<invalid>"),
                    file_range,
                    section.address(),
                    section.size()
                ));
                entries.push(InspectElfEntry::Section {
                    name: section.name().unwrap_or("<invalid>").to_owned(),
                    file_offset: file_range.map(|range| range.0),
                    file_size: file_range.map(|range| range.1),
                    virtual_address: section.address(),
                    size: section.size(),
                });
            }
        }
        InspectElfQuery::Segments => {
            for segment in file.segments().take(100) {
                let (file_offset, file_size) = segment.file_range();
                model.push_str(&format!(
                    "{} file {file_offset}+{file_size} virtual {}+{}\n",
                    segment.name().ok().flatten().unwrap_or("<unnamed>"),
                    segment.address(),
                    segment.size()
                ));
                entries.push(InspectElfEntry::Segment {
                    name: segment.name().ok().flatten().map(str::to_owned),
                    file_offset,
                    file_size,
                    virtual_address: segment.address(),
                    size: segment.size(),
                });
            }
        }
        InspectElfQuery::Symbols(literal) => {
            for symbol in file.symbols().chain(file.dynamic_symbols()) {
                if entries.len() >= 100 {
                    break;
                }
                if let Ok(symbol_name) = symbol.name()
                    && !symbol_name.is_empty()
                    && literal
                        .as_deref()
                        .is_none_or(|value| symbol_name.contains(value))
                {
                    model.push_str(&format!(
                        "{symbol_name} virtual {}+{}\n",
                        symbol.address(),
                        symbol.size()
                    ));
                    entries.push(InspectElfEntry::Symbol {
                        name: symbol_name.to_owned(),
                        virtual_address: symbol.address(),
                        size: symbol.size(),
                    });
                }
            }
        }
        InspectElfQuery::Relocations(literal) => {
            for section in file.sections() {
                let section_name = section.name().unwrap_or("<invalid>");
                for (offset, relocation) in section.relocations() {
                    let target = format!("{:?}", relocation.target());
                    if literal
                        .as_deref()
                        .is_some_and(|value| !target.contains(value) && !section_name.contains(value))
                    {
                        continue;
                    }
                    model.push_str(&format!(
                        "{section_name} {offset} {target} {:?}\n",
                        relocation.kind()
                    ));
                    entries.push(InspectElfEntry::Relocation {
                        section: section_name.to_owned(),
                        offset,
                        target,
                        kind: format!("{:?}", relocation.kind()),
                    });
                    if entries.len() >= 100 {
                        break;
                    }
                }
                if entries.len() >= 100 {
                    break;
                }
            }
        }
        InspectElfQuery::Dynamic(literal) => {
            let imports = file
                .imports()
                .map_err(|error| format!("failed to inspect dynamic imports in {name}: {error}"))?;
            for import in imports {
                let import_name = String::from_utf8_lossy(import.name()).into_owned();
                let library = String::from_utf8_lossy(import.library()).into_owned();
                if literal
                    .as_deref()
                    .is_some_and(|value| !import_name.contains(value) && !library.contains(value))
                {
                    continue;
                }
                model.push_str(&format!("{import_name} {library}\n"));
                entries.push(InspectElfEntry::Dynamic {
                    name: import_name,
                    value: library,
                });
                if entries.len() >= 100 {
                    break;
                }
            }
        }
        InspectElfQuery::Address(address) => {
            for section in file.sections() {
                let start = section.address();
                let end = start.saturating_add(section.size());
                if *address >= start
                    && *address < end
                    && let Some((file_offset, _)) = section.file_range()
                {
                    let mapped = file_offset.saturating_add(*address - start);
                    model.push_str(&format!("virtual {address} -> file {mapped}\n"));
                    entries.push(InspectElfEntry::Mapping {
                        file_offset: mapped,
                        virtual_address: *address,
                    });
                    break;
                }
            }
        }
        InspectElfQuery::Offset(offset) => {
            for section in file.sections() {
                let Some((file_start, file_size)) = section.file_range() else {
                    continue;
                };
                let file_end = file_start.saturating_add(file_size);
                if *offset >= file_start && *offset < file_end {
                    let mapped = section.address().saturating_add(*offset - file_start);
                    model.push_str(&format!("file {offset} -> virtual {mapped}\n"));
                    entries.push(InspectElfEntry::Mapping {
                        file_offset: *offset,
                        virtual_address: mapped,
                    });
                    break;
                }
            }
        }
    }
    if model.is_empty() {
        model.push_str("no results\n");
        entries.push(InspectElfEntry::Notice("no results".to_owned()));
    }
    Ok(InspectCommandOutput {
        model,
        result: InspectJobSuccess::Elf(InspectElfResult {
            path: name,
            query: request.query.clone(),
            summary,
            entries,
        }),
    })
}

fn parse_number(value: &str) -> Result<u64, String> {
    let parsed = if let Some(hex) = value.strip_prefix("0x") {
        u64::from_str_radix(hex, 16)
    } else {
        value.parse()
    };
    parsed.map_err(|_| {
        format!("failed to parse `inspect` elf input: `{value}` is not an integer or hex value")
    })
}
