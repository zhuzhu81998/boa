//! A narrow pre-link hook. Cargo archives are rewritten only in a temporary directory.
mod transform;

use object::read::archive::ArchiveFile;
use std::{
    collections::BTreeSet,
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

fn main() -> ExitCode {
    match run(env::args_os().skip(1).collect()) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("boa-jit-linker: {error}");
            ExitCode::FAILURE
        }
    }
}

fn bitcode(bytes: &[u8]) -> bool {
    bytes.starts_with(b"BC\xc0\xde") || bytes.starts_with(&[0xde, 0xc0, 0x17, 0x0b])
}

fn run(mut args: Vec<OsString>) -> Result<u8, String> {
    // rustc's ordinary Unix linker arguments are direct arguments, not shell commands.
    // Fail explicitly for JIT response-file links until their driver-specific syntax is supported.
    let jit = args.iter().any(|arg| arg == "-lboa_jit_resolver");
    let linker =
        env::var_os("BOA_JIT_REAL_LINKER").unwrap_or_else(|| "/usr/lib/llvm-22/bin/clang".into());
    let temp = tempfile::tempdir().map_err(|e| e.to_string())?;
    if jit {
        if args
            .iter()
            .any(|arg| arg.to_string_lossy().starts_with('@'))
        {
            return Err("response-file JIT links are not supported yet".into());
        }
        let mut directories = Vec::new();
        for (i, arg) in args.iter().enumerate() {
            if arg == "-L" {
                directories.push(PathBuf::from(args.get(i + 1).ok_or("missing -L argument")?));
            } else if let Some(path) = arg.to_str().and_then(|s| s.strip_prefix("-L")) {
                directories.push(PathBuf::from(path));
            }
        }
        // Follow the linker's search order so the request matches the library being replaced.
        let directory = directories
            .iter()
            .find(|dir| dir.join("libboa_jit_resolver.a").is_file())
            .ok_or("cannot locate libboa_jit_resolver.a in linker search paths")?;
        let request_archive = fs::read(directory.join("libboa_jit_resolver.a"))
            .map_err(|e| format!("cannot read resolver request: {e}"))?;
        let archive = ArchiveFile::parse(request_archive.as_slice()).map_err(|e| e.to_string())?;
        let mut requests = Vec::new();
        for member in archive.members() {
            let member = member.map_err(|e| e.to_string())?;
            let data = member
                .data(request_archive.as_slice())
                .map_err(|e| e.to_string())?;
            if bitcode(data) {
                requests.push(data);
            }
        }
        if requests.len() != 1 {
            return Err("expected one bitcode request in resolver archive".into());
        }
        let request = requests[0];
        let mut transformed = 0;
        for (index, arg) in args.iter_mut().enumerate() {
            let path = PathBuf::from(&*arg);
            let extension = path.extension().and_then(|s| s.to_str());
            if !matches!(extension, Some("o" | "rlib" | "a")) || !path.is_file() {
                continue;
            }
            let bytes = fs::read(&path).map_err(|e| e.to_string())?;
            if bitcode(&bytes) {
                if let Some(output) = transform::inject(&bytes, &request)? {
                    // Keep the .o suffix: Clang must forward bitcode to LLD, not compile .bc
                    // itself before the linker's LTO options can take effect.
                    let dest = temp.path().join(format!("runtime-{index}.o"));
                    fs::write(&dest, output).map_err(|e| e.to_string())?;
                    *arg = dest.into_os_string();
                    transformed += 1;
                }
            } else if bytes.starts_with(b"!<arch>\n") {
                let archive = ArchiveFile::parse(bytes.as_slice()).map_err(|e| e.to_string())?;
                let mut replacements = Vec::new();
                let mut names = BTreeSet::new();
                for member in archive.members() {
                    let member = member.map_err(|e| e.to_string())?;
                    let name = std::str::from_utf8(member.name()).map_err(|e| e.to_string())?;
                    if !names.insert(name.to_owned()) {
                        return Err(format!("duplicate archive member {name}"));
                    }
                    let data = member.data(bytes.as_slice()).map_err(|e| e.to_string())?;
                    if bitcode(data) {
                        if let Some(output) = transform::inject(data, &request)? {
                            if Path::new(name).file_name().and_then(|s| s.to_str()) != Some(name) {
                                return Err(
                                    "runtime archive member must have a plain filename".into()
                                );
                            }
                            replacements.push((name.to_owned(), output));
                        }
                    }
                }
                if replacements.is_empty() {
                    continue;
                }
                let dir = temp.path().join(format!("archive-{index}"));
                fs::create_dir(&dir).map_err(|e| e.to_string())?;
                let dest = dir.join("runtime.a");
                fs::write(&dest, bytes).map_err(|e| e.to_string())?;
                let mut command = Command::new(
                    env::var_os("LLVM_AR").unwrap_or_else(|| "/usr/lib/llvm-22/bin/llvm-ar".into()),
                );
                command.arg("rs").arg(&dest);
                for (name, output) in replacements {
                    let member = dir.join(name);
                    fs::write(&member, output).map_err(|e| e.to_string())?;
                    command.arg(member);
                    transformed += 1;
                }
                if !command.status().map_err(|e| e.to_string())?.success() {
                    return Err("llvm-ar failed".into());
                }
                *arg = dest.into_os_string();
            }
        }
        if transformed != 1 {
            return Err(format!(
                "expected one runtime module with handlers, found {transformed}; use linker-plugin LTO and one engine codegen unit"
            ));
        }
        args.retain(|arg| arg != "-lboa_jit_resolver");
        eprintln!("boa-jit-linker: inserted address table into runtime bitcode");
    }
    let status = Command::new(linker)
        .args(args)
        .status()
        .map_err(|e| e.to_string())?;
    Ok(status.code().unwrap_or(1).try_into().unwrap_or(1))
}
