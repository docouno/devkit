use crate::{Result, distribution::Target};
use anyhow::{Context, ensure};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
};

/// Inspect the same open file used for SHA256. Host and container targets are
/// selected by the caller, so foreign contributions never need to execute.
pub(super) fn verify_format(file: &mut File, target: Target) -> Result<()> {
    file.rewind()?;
    if let Some(machine) = target.elf_machine() {
        verify_elf(file, machine)
    } else {
        verify_mach_o(file)
    }
}

fn word16(bytes: &[u8]) -> u16 {
    u16::from_le_bytes(bytes[..2].try_into().unwrap())
}
fn word32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes(bytes[..4].try_into().unwrap())
}
fn word64(bytes: &[u8]) -> u64 {
    u64::from_le_bytes(bytes[..8].try_into().unwrap())
}

fn verify_elf(file: &mut File, machine: u16) -> Result<()> {
    let file_len = file.metadata()?.len();
    let mut header = [0_u8; 64];
    file.read_exact(&mut header)
        .context("Truncated native ELF header")?;
    ensure!(
        &header[..7] == b"\x7fELF\x02\x01\x01"
            && matches!(word16(&header[16..]), 2 | 3)
            && word16(&header[18..]) == machine
            && word32(&header[20..]) == 1,
        "Expected a 64-bit Linux executable with ELF machine {machine}"
    );
    let offset = word64(&header[32..]);
    let entry_size = u64::from(word16(&header[54..]));
    let count = u64::from(word16(&header[56..]));
    ensure!(
        word16(&header[52..]) == 64
            && offset >= 64
            && entry_size >= 56
            && (1..=128).contains(&count)
            && offset
                .checked_add(entry_size * count)
                .is_some_and(|end| end <= file_len),
        "Invalid native ELF program headers"
    );
    let mut executable_segment = false;
    for n in 0..count {
        file.seek(SeekFrom::Start(offset + n * entry_size))?;
        let mut record = [0_u8; 56];
        file.read_exact(&mut record)
            .context("Truncated native ELF")?;
        let kind = word32(&record);
        let start = word64(&record[8..]);
        let size = word64(&record[32..]);
        ensure!(
            start.checked_add(size).is_some_and(|end| end <= file_len),
            "Truncated ELF segment"
        );
        ensure!(kind != 3, "Native engine has a dynamic program interpreter");
        if kind == 1 && word32(&record[4..]) & 1 != 0 && size > 0 {
            executable_segment = true;
        }
        if kind == 2 {
            ensure!(
                size.is_multiple_of(16) && size <= 1024 * 1024,
                "Invalid ELF dynamic segment"
            );
            file.seek(SeekFrom::Start(start))?;
            let mut dynamic = vec![0_u8; size as usize];
            file.read_exact(&mut dynamic)?;
            for item in dynamic.as_chunks::<16>().0 {
                ensure!(
                    word64(item) != 1,
                    "Native engine requires a dynamic library"
                );
            }
        }
    }
    ensure!(executable_segment, "ELF has no executable load segment");
    Ok(())
}

/// A Mach-O lc_str is relative to its command and must terminate inside it.
fn command_path(command: &[u8], fixed_size: usize) -> Result<&str> {
    ensure!(command.len() >= fixed_size, "Truncated Mach-O path command");
    let offset = word32(&command[8..]) as usize;
    ensure!(
        (fixed_size..command.len()).contains(&offset),
        "Invalid Mach-O path offset"
    );
    let path = &command[offset..];
    let end = path
        .iter()
        .position(|&b| b == 0)
        .context("Unterminated Mach-O path")?;
    Ok(std::str::from_utf8(&path[..end])?)
}

fn verify_mach_o(file: &mut File) -> Result<()> {
    const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xfade0cc0;
    let file_len = file.metadata()?.len();
    let mut header = [0_u8; 32];
    file.read_exact(&mut header)
        .context("Truncated native Mach-O header")?;
    ensure!(
        word32(&header) == 0xfeedfacf
            && word32(&header[4..]) == 0x0100000c
            && word32(&header[12..]) == 2,
        "Expected a thin ARM64 Mach-O executable"
    );
    let count = word32(&header[16..]);
    let length = word32(&header[20..]) as usize;
    let data_start = 32 + length as u64;
    ensure!(
        (1..=1024).contains(&count) && length <= 1024 * 1024 && data_start <= file_len,
        "Invalid Mach-O load command table"
    );
    let mut commands = vec![0; length];
    file.read_exact(&mut commands)?;
    let mut rest = commands.as_slice();
    let mut linker = false;
    let mut executable_segment = false;
    let mut signature: Option<(u64, u64)> = None;
    for _ in 0..count {
        ensure!(rest.len() >= 8, "Truncated Mach-O load command");
        let size = word32(&rest[4..]) as usize;
        ensure!(
            (8..=rest.len()).contains(&size) && size.is_multiple_of(8),
            "Invalid Mach-O command size"
        );
        let command;
        (command, rest) = rest.split_at(size);
        match word32(command) {
            // LC_SEGMENT_64
            0x19 => {
                ensure!(size >= 72, "Truncated Mach-O segment command");
                let fileoff = word64(&command[40..]);
                let filesize = word64(&command[48..]);
                let initprot = word32(&command[60..]);
                let nsects = word32(&command[64..]) as usize;
                ensure!(
                    fileoff
                        .checked_add(filesize)
                        .is_some_and(|end| end <= file_len)
                        && nsects
                            .checked_mul(80)
                            .and_then(|bytes| bytes.checked_add(72))
                            .is_some_and(|end| end <= size),
                    "Invalid Mach-O segment bounds"
                );
                // VM_PROT_EXECUTE == 0x4
                if initprot & 0x4 != 0 && filesize > 0 {
                    executable_segment = true;
                }
            }
            // LC_LOAD_DYLINKER
            0xe => {
                ensure!(
                    !linker && command_path(command, 12)? == "/usr/lib/dyld",
                    "Unexpected Mach-O dynamic linker"
                );
                linker = true;
            }
            // LOAD, LOAD_WEAK, REEXPORT, LAZY_LOAD and LOAD_UPWARD_DYLIB
            0xc | 0x80000018 | 0x8000001f | 0x20 | 0x80000023 => {
                let path = command_path(command, 24)?;
                ensure!(
                    (path.starts_with("/usr/lib/")
                        || path.starts_with("/System/Library/Frameworks/"))
                        && path[1..]
                            .split('/')
                            .all(|part| !matches!(part, "" | "." | "..")),
                    "Unexpected Mach-O dylib: {path}"
                );
            }
            0x8000001c | 0x27 => {
                anyhow::bail!("Mach-O contains a runtime search path or dyld environment command")
            }
            // LC_CODE_SIGNATURE
            0x1d => {
                ensure!(
                    signature.is_none() && size == 16,
                    "Invalid Mach-O code signature command"
                );
                let offset = u64::from(word32(&command[8..]));
                let sig_size = u64::from(word32(&command[12..]));
                ensure!(
                    offset >= data_start
                        && sig_size >= 12
                        && offset
                            .checked_add(sig_size)
                            .is_some_and(|end| end <= file_len),
                    "Invalid Mach-O code signature range"
                );
                signature = Some((offset, sig_size));
            }
            _ => {}
        }
    }
    ensure!(
        rest.is_empty() && linker && executable_segment,
        "Mach-O requires a complete command table, executable segment, system linker and code signature"
    );
    let (sig_offset, sig_size) = signature.context("Mach-O requires a code signature")?;
    let mut superblob = [0_u8; 8];
    file.seek(SeekFrom::Start(sig_offset))?;
    file.read_exact(&mut superblob)
        .context("Truncated Mach-O code signature blob")?;
    let magic = u32::from_be_bytes(superblob[..4].try_into().unwrap());
    let blob_len = u64::from(u32::from_be_bytes(superblob[4..8].try_into().unwrap()));
    ensure!(
        magic == CSMAGIC_EMBEDDED_SIGNATURE && (12..=sig_size).contains(&blob_len),
        "Invalid Mach-O embedded code signature"
    );
    Ok(())
}
