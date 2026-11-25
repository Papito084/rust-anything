//! Step 5: the definition of done for one port.
//!
//! fmt, clippy (warnings are errors), cargo tests, no unexplained `unsafe`, every function
//! ported, the built module exposes the original API, and the recorded behavior replays cleanly.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use serde::Serialize;

use crate::bridge::Manifest;
use crate::harness::{self, CASES, DiffOptions};

#[derive(Debug, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub passed: bool,
    pub detail: String,
    /// The next command or edit that clears a failed check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
}

/// Recovery guidance when an outside tool failed to start (structured `SpawnError`).
fn spawn_fix(err: &anyhow::Error) -> Option<String> {
    let spawn = err.downcast_ref::<crate::process::SpawnError>()?;
    Some(if spawn.missing {
        match crate::doctor::missing_command(&spawn.program) {
            Some(install) if !install.contains('\n') => {
                format!(
                    "install `{}` with `{install}`, or run `rust-anything doctor`",
                    spawn.program
                )
            }
            _ => format!(
                "run `rust-anything doctor` for how to install `{}`",
                spawn.program
            ),
        }
    } else {
        "run `rust-anything doctor` and check that the build tool can run".into()
    })
}

/// The fix for a failed check, by its name.
fn fix_for(check: &Check, crate_dir: &Path) -> Option<String> {
    let c = crate_dir.display();
    let manifest = format!("--manifest-path {c}/Cargo.toml");
    Some(match check.name {
        "fmt" => format!("cargo fmt {manifest}"),
        "clippy" => {
            format!("cargo clippy --all-targets {manifest} -- -D warnings, then fix each warning")
        }
        "cargo test" => format!("cargo test {manifest}"),
        "unsafe" => "add a `// SAFETY:` comment above each listed `unsafe`, or remove it".into(),
        "ported" => {
            "port the functions still stubbed in src/port.rs, then `rust-anything gate` again"
                .into()
        }
        "size" => "use the crates `rust-anything analyze` lists and fold repeated code".into(),
        "build" => format!("fix the compile error above, then `rust-anything build {c}`"),
        "recording" => format!("rust-anything record {c}"),
        "behavior" if check.detail.contains("no recorded cases") => {
            format!("rust-anything record {c}")
        }
        "behavior" => format!(
            "rust-anything diff {c} to see each mismatch; fix the port, or list an intended difference in {}",
            crate::recording::KNOWN_DIFFS
        ),
        "known diffs" => format!(
            "remove the entries that no longer happen from {c}/{}",
            crate::recording::KNOWN_DIFFS
        ),
        "api" => {
            format!("export the missing functions from src/port.rs, then `rust-anything build {c}`")
        }
        "error cases" | "panics" => {
            format!("rust-anything diff {c} to see the cases where the port's errors differ")
        }
        _ => return None,
    })
}

/// Bindings `bridge` writes into `src/`; everything else there is the port.
const GENERATED: [&str; 8] = [
    "lib.rs",
    "js.rs",
    "pinned.rs",
    "pyconv.rs",
    "pynum.rs",
    "rbconv.rs",
    "utf16.rs",
    "wire.rs",
];

/// How many times the source's lines the port may take before `gate` calls it bloated, plus a
/// fixed allowance for the imports and signatures every port has.
pub const MAX_GROWTH: f64 = 3.0;
const ALLOWANCE: f64 = 50.0;

fn files_under(path: &Path, extensions: &[&str]) -> Vec<std::path::PathBuf> {
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    crate::walk::builder(path)
        .build()
        .flatten()
        .map(ignore::DirEntry::into_path)
        .filter(|p| {
            p.extension()
                .is_some_and(|e| extensions.contains(&e.to_string_lossy().as_ref()))
        })
        .collect()
}

/// Lines of Rust in the port against lines of the source unit: a port several times larger
/// usually re-implements what a crate already does.
/// The unit's files and the repository files they import, directly or not: a port stands in
/// for all of them.
pub(crate) fn with_local_imports(root: &Path, files: Vec<PathBuf>) -> Vec<PathBuf> {
    const TRIES: &[&str] = &[
        "",
        ".ts",
        ".tsx",
        ".mts",
        ".cts",
        ".js",
        ".mjs",
        ".cjs",
        "/index.ts",
        "/index.tsx",
        "/index.js",
        "/index.mjs",
    ];
    let resolve = |from: &Path, spec: &str| -> Option<PathBuf> {
        let base = from.parent()?.join(spec);
        let stem = base.with_extension("");
        let js_to_ts = matches!(
            base.extension().and_then(|e| e.to_str()),
            Some("js" | "mjs" | "cjs")
        );
        TRIES
            .iter()
            .map(|t| PathBuf::from(format!("{}{t}", base.display())))
            .chain(js_to_ts.then(|| stem.with_extension("ts")))
            .find(|p| p.is_file())
    };
    let mut seen: std::collections::BTreeSet<PathBuf> = files.into_iter().collect();
    let mut todo: Vec<PathBuf> = seen.iter().cloned().collect();
    while let Some(file) = todo.pop() {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for import in crate::ts::scan::scan(&file.to_string_lossy(), &text).imports {
            if !import.specifier.starts_with('.') || import.type_only {
                continue;
            }
            if let Some(found) = resolve(&file, &import.specifier)
                && found.starts_with(root)
                && !found.components().any(|c| c.as_os_str() == "node_modules")
                && seen.insert(found.clone())
            {
                todo.push(found);
            }
        }
    }
    seen.into_iter().collect()
}

fn size_check(crate_dir: &Path, manifest: &Manifest) -> Option<Check> {
    let mut files = files_under(
        &manifest.root.join(&manifest.unit),
        manifest.language.extensions(),
    );
    if manifest.language == crate::lang::Lang::Ts {
        files = with_local_imports(&manifest.root, files);
    }
    if manifest.language == crate::lang::Lang::C {
        let ported: Vec<&str> = manifest.exports.iter().map(|f| f.name.as_str()).collect();
        files.extend(crate::lang::c::called_implementations(
            &manifest.root,
            &manifest.unit,
            &ported,
        ));
    }
    let source: usize = files
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .map(|t| crate::lang::syntax::loc(&t, manifest.language))
        .sum();
    if source == 0 {
        return None;
    }
    let rust: usize = files_under(&crate_dir.join("src"), &["rs"])
        .iter()
        .filter(|p| {
            p.file_name()
                .is_none_or(|n| !GENERATED.contains(&n.to_string_lossy().as_ref()))
        })
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .map(|t| crate::lang::syntax::code_lines(&t, &["//"]))
        .sum();
    let growth = rust as f64 / source as f64;
    let passed = rust as f64 <= MAX_GROWTH * source as f64 + ALLOWANCE;
    Some(Check {
        fix: None,
        name: "size",
        passed,
        detail: format!(
            "{} of Rust for {} of {} ({growth:.1}x){}",
            crate::report::count(rust, "line"),
            crate::report::count(source, "line"),
            manifest.language.name(),
            if passed {
                String::new()
            } else {
                format!(
                    "; over {MAX_GROWTH}x usually means code a crate already provides, or a case \
                     handled one by one: use the crates `analyze` lists and fold repeated code"
                )
            }
        ),
    })
}

#[derive(Debug, Serialize)]
pub struct GateOutcome {
    pub passed: bool,
    pub checks: Vec<Check>,
}

pub fn gate(crate_dir: &Path, node_args: &[String]) -> Result<GateOutcome> {
    let manifest = Manifest::load(crate_dir)?;
    let lockfile = lockfile_check(crate_dir);
    let mut checks = rust_checks(crate_dir, &crate::cache::target_for(crate_dir)?.dir);
    checks.extend(lockfile);
    checks.extend(advisories_check(crate_dir));
    checks.push(ported_check(crate_dir));

    checks.extend(size_check(crate_dir, &manifest));
    checks.extend(crate::error_paths::checks(crate_dir, &manifest));
    let built = harness::build(crate_dir);
    let built_ok = built.is_ok();
    let (build_detail, build_fix) = match built {
        Ok(path) => (format!("built {}", path.display()), None),
        Err(e) => (last_lines(&format!("{e:#}"), 15), spawn_fix(&e)),
    };
    checks.push(Check {
        fix: build_fix,
        name: "build",
        passed: built_ok,
        detail: build_detail,
    });

    if let Some(why) = crate::recording::stale(crate_dir, &manifest) {
        checks.push(Check {
            fix: None,
            name: "recording",
            passed: false,
            detail: why,
        });
    }

    if let Some(line) = crate::recording::saved_coverage(crate_dir)
        .and_then(|c| crate::recording::coverage_line(&c))
    {
        checks.push(Check {
            fix: None,
            name: "coverage",
            passed: true,
            detail: line,
        });
    }

    if !crate_dir.join(CASES).exists() {
        checks.push(Check {
            fix: None,
            name: "behavior",
            passed: false,
            detail: "no recorded cases: run `rust-anything record` first".into(),
        });
    } else if built_ok {
        match harness::diff(&DiffOptions {
            crate_dir,
            module: None,
            ignore_key_order: false,
            float_tolerance: None,
            node_args,
        }) {
            Ok(d) => {
                checks.push(Check {
                    fix: None,
                    name: "api",
                    passed: d.missing.is_empty(),
                    detail: if d.missing.is_empty() {
                        format!(
                            "exports {}",
                            manifest
                                .exports
                                .iter()
                                .map(|f| f.name.clone())
                                .chain(manifest.classes.iter().map(|c| format!("class {}", c.name)))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    } else {
                        format!("missing exports: {}", d.missing.join(", "))
                    },
                });
                checks.push(Check {
                    fix: None,
                    name: "behavior",
                    passed: d.failed == 0,
                    detail: if d.accepted > 0 {
                        format!(
                            "{}/{} recorded cases match ({} by {})",
                            d.passed,
                            d.total,
                            d.accepted,
                            crate::recording::KNOWN_DIFFS
                        )
                    } else {
                        format!("{}/{} recorded cases match", d.passed, d.total)
                    },
                });
                if !d.stale_known.is_empty() {
                    checks.push(Check {
                        fix: None,
                        name: "known diffs",
                        passed: false,
                        detail: format!(
                            "{} entr{} of {} no longer differ{}: remove {}",
                            d.stale_known.len(),
                            if d.stale_known.len() == 1 { "y" } else { "ies" },
                            crate::recording::KNOWN_DIFFS,
                            if d.stale_known.len() == 1 { "s" } else { "" },
                            d.stale_known
                                .iter()
                                .map(|e| format!(
                                    "{}({})",
                                    e["fn"].as_str().unwrap_or("?"),
                                    e["args"]
                                ))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    });
                }
            }
            Err(e) => checks.push(Check {
                fix: None,
                name: "behavior",
                passed: false,
                detail: format!("{e:#}"),
            }),
        }
    }

    for check in checks.iter_mut().filter(|c| !c.passed && c.fix.is_none()) {
        check.fix = fix_for(check, crate_dir);
    }
    Ok(GateOutcome {
        passed: checks.iter().all(|c| c.passed),
        checks,
    })
}

fn lockfile_check(crate_dir: &Path) -> Option<Check> {
    if !crate_dir.join("Cargo.lock").exists() {
        return Some(Check {
            fix: None,
            name: "lockfile",
            passed: true,
            detail: "warning: no Cargo.lock yet; keep the one the build writes, so builds are reproducible".into(),
        });
    }
    let mut cmd = Command::new("cargo");
    cmd.args([
        "metadata",
        "--locked",
        "--offline",
        "--format-version",
        "1",
        "--manifest-path",
    ])
    .arg(crate_dir.join("Cargo.toml"));
    let out = crate::process::output(&mut cmd, crate::process::BUILD).ok()?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stale = !out.status.success() && stderr.contains("--locked");
    Some(Check {
        fix: None,
        name: "lockfile",
        passed: true,
        detail: if stale {
            "warning: Cargo.lock is out of date with Cargo.toml; keep the refreshed one the build wrote".into()
        } else {
            "Cargo.lock matches Cargo.toml".into()
        },
    })
}

fn advisories_check(crate_dir: &Path) -> Option<Check> {
    let manifest = crate_dir.join("Cargo.toml");
    let attempts: [(&str, Vec<std::ffi::OsString>); 2] = [
        (
            "cargo-deny",
            vec![
                "deny".into(),
                "--manifest-path".into(),
                manifest.clone().into(),
                "check".into(),
                "advisories".into(),
            ],
        ),
        (
            "cargo-audit",
            vec![
                "audit".into(),
                "--file".into(),
                crate_dir.join("Cargo.lock").into(),
            ],
        ),
    ];
    for (tool, args) in attempts {
        if !crate::setup::on_path(tool) {
            continue;
        }
        let mut cmd = Command::new("cargo");
        cmd.args(&args);
        let out = crate::process::output(&mut cmd, crate::process::BUILD).ok()?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        return Some(Check {
            fix: None,
            name: "advisories",
            passed: true,
            detail: if out.status.success() {
                format!("no known advisories ({tool})")
            } else {
                format!(
                    "warning: {tool} reports advisories\n{}",
                    last_lines(&text, 15)
                )
            },
        });
    }
    None
}

/// The checks that hold for any Rust crate, bindings or not: fmt, clippy (warnings are errors),
/// cargo test, and `unsafe` only with a `// SAFETY:` comment.
pub fn rust_checks(crate_dir: &Path, target_dir: &Path) -> Vec<Check> {
    let cargo = |args: &[&str]| -> (bool, String, Option<String>) {
        let mut cmd = Command::new("cargo");
        cmd.args(args)
            .arg("--manifest-path")
            .arg(crate_dir.join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", target_dir);
        // Lint arguments go after `--`, so the manifest path must come before them.
        match crate::process::output(&mut cmd, crate::process::BUILD) {
            Ok(o) => (
                o.status.success(),
                last_lines(&String::from_utf8_lossy(&o.stderr), 15),
                None,
            ),
            Err(e) => (false, format!("{e:#}"), spawn_fix(&e)),
        }
    };
    let mut checks = Vec::new();

    let (ok, out, fix) = cargo(&["fmt", "--check"]);
    checks.push(Check {
        name: "fmt",
        passed: ok,
        detail: if ok {
            "formatted".into()
        } else if fix.is_some() {
            out
        } else {
            format!("run `cargo fmt`\n{out}")
        },
        fix,
    });

    let mut clippy = Command::new("cargo");
    clippy
        .args(["clippy", "--all-targets", "--manifest-path"])
        .arg(crate_dir.join("Cargo.toml"))
        .args(["--", "-D", "warnings"])
        .env("CARGO_TARGET_DIR", target_dir);
    let (ok, out, fix) = match crate::process::output(&mut clippy, crate::process::BUILD) {
        Ok(o) => (
            o.status.success(),
            last_lines(&String::from_utf8_lossy(&o.stderr), 15),
            None,
        ),
        Err(e) => (false, format!("{e:#}"), spawn_fix(&e)),
    };
    checks.push(Check {
        fix,
        name: "clippy",
        passed: ok,
        detail: if ok { "no warnings".into() } else { out },
    });

    let (ok, out, fix) = cargo(&["test", "--quiet"]);
    checks.push(Check {
        fix,
        name: "cargo test",
        passed: ok,
        detail: if ok { "passed".into() } else { out },
    });

    let (unexplained, explained) = unsafe_blocks(&crate_dir.join("src"));
    checks.push(Check {
        fix: None,
        name: "unsafe",
        passed: unexplained.is_empty(),
        detail: if unexplained.is_empty() {
            format!("{explained} unsafe uses, each with a `// SAFETY:` comment")
        } else {
            format!(
                "unsafe without a `// SAFETY:` comment: {}",
                unexplained.join(", ")
            )
        },
    });

    checks
}

/// No function in `src/port.rs` is still `unimplemented!`.
pub fn ported_check(crate_dir: &Path) -> Check {
    let path = crate_dir.join("src/port.rs");
    let port = match std::fs::read_to_string(&path) {
        Ok(port) => port,
        Err(error) => {
            return Check {
                fix: None,
                name: "ported",
                passed: false,
                detail: format!("cannot read {}: {error}", path.display()),
            };
        }
    };
    let todo = stubs(&port);
    Check {
        fix: None,
        name: "ported",
        passed: todo == 0,
        detail: if todo == 0 {
            "nothing in port.rs is `unimplemented!`, `todo!` or only a `panic!`".into()
        } else {
            format!(
                "{todo} place(s) in port.rs still `unimplemented!`, `todo!` or a function that only panics"
            )
        },
    }
}

/// Stubs left in a port: `unimplemented!`, `todo!`, and functions whose whole body is a `panic!`.
fn stubs(port: &str) -> usize {
    let code: String = port
        .lines()
        .map(|l| l.split("//").next().unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    let macros = code.matches("unimplemented!(").count() + code.matches("todo!(").count();
    let panics = regex::Regex::new(r"\bfn\s+\w+[^{;]*\{\s*panic!\([^;{}]*\)\s*;?\s*\}")
        .map(|re| re.find_iter(&code).count())
        .unwrap_or(0);
    macros + panics
}

/// `unsafe` uses in Rust sources, split into those without and with a `// SAFETY:` comment just above.
fn unsafe_blocks(src: &Path) -> (Vec<String>, usize) {
    let mut unexplained = Vec::new();
    let mut explained = 0;
    let mut stack = vec![src.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let lines: Vec<&str> = text.lines().collect();
            for (i, line) in lines.iter().enumerate() {
                let code = strip_code(line);
                let uses_unsafe = code
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .any(|word| word == "unsafe");
                if !uses_unsafe {
                    continue;
                }
                let has_safety = lines[..i]
                    .iter()
                    .rev()
                    .take_while(|l| l.trim_start().starts_with("//"))
                    .any(|l| l.contains("SAFETY:"));
                if has_safety {
                    explained += 1;
                } else {
                    unexplained.push(format!("{}:{}", path.display(), i + 1));
                }
            }
        }
    }
    (unexplained, explained)
}

fn last_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Strips string and char literals and comments from a line of Rust source code so keyword
/// searches (like `unsafe`) only match real code, not strings or comments.
fn strip_code(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while i < bytes.len() {
        // Line comment: everything from here to end of line is ignored.
        if bytes[i..].starts_with(b"//") {
            break;
        }

        // Block comment: /* ... */
        if bytes[i..].starts_with(b"/*") {
            i += 2;
            while i < bytes.len() && !bytes[i..].starts_with(b"*/") {
                i += 1;
            }
            if i < bytes.len() {
                i += 2;
            }
            out.push(' ');
            continue;
        }

        // Raw string: optional 'b' or 'c', then 'r', zero or more '#', then '"'.
        let prev_is_ident = i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_');
        let mut j = i;
        if !prev_is_ident
            && (bytes[j] == b'b' || bytes[j] == b'c')
            && j + 1 < bytes.len()
            && bytes[j + 1] == b'r'
        {
            j += 1;
        }
        if !prev_is_ident && bytes[j] == b'r' {
            let mut k = j + 1;
            while k < bytes.len() && bytes[k] == b'#' {
                k += 1;
            }
            if k < bytes.len() && bytes[k] == b'"' {
                let hashes = k - (j + 1);
                i = k + 1;
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        let rest = &bytes[i + 1..];
                        if rest.len() >= hashes && rest[..hashes].iter().all(|&b| b == b'#') {
                            i += 1 + hashes;
                            break;
                        }
                    }
                    i += 1;
                }
                out.push(' ');
                continue;
            }
        }

        // Standard string: optional 'b' or 'c', then '"'.
        let is_byte_or_c_str = !prev_is_ident
            && (bytes[i] == b'b' || bytes[i] == b'c')
            && i + 1 < bytes.len()
            && bytes[i + 1] == b'"';
        if bytes[i] == b'"' || is_byte_or_c_str {
            if is_byte_or_c_str {
                i += 1;
            }
            i += 1; // skip opening '"'
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i += if i + 1 < bytes.len() { 2 } else { 1 };
                } else if bytes[i] == b'"' {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
            out.push(' ');
            continue;
        }

        // Char literal: 'x', '\n', '\'', etc.
        let is_byte_char =
            !prev_is_ident && bytes[i] == b'b' && i + 1 < bytes.len() && bytes[i + 1] == b'\'';
        if bytes[i] == b'\'' || is_byte_char {
            let quote_pos = if is_byte_char { i + 1 } else { i };
            let rest = &line[quote_pos + 1..];
            if let Some(close_rel) = rest.find('\'')
                && close_rel <= 8
                && !rest[..close_rel].contains(' ')
            {
                i = quote_pos + 1 + close_rel + 1;
                out.push(' ');
                continue;
            }
        }

        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Check, fix_for, spawn_fix, stubs, unsafe_blocks};

    #[test]
    fn counts_every_kind_of_stub() {
        let port = "pub fn a() -> u8 { todo!() }\n\
                    pub fn b() -> u8 { unimplemented!(\"later\") }\n\
                    pub fn c(x: u8) -> u8 {\n    panic!(\"not ported\")\n}\n\
                    pub fn d(x: u8) -> u8 { if x > 1 { panic!(\"big\") } x }\n\
                    // todo!() in a comment\n";
        assert_eq!(stubs(port), 3);
        assert_eq!(stubs("pub fn ok() -> u8 { 1 }"), 0);
    }

    #[test]
    fn failed_checks_say_what_to_run() {
        let check = |name, detail: &str| Check {
            fix: None,
            name,
            passed: false,
            detail: detail.into(),
        };
        let dir = std::path::Path::new("port");
        assert_eq!(
            fix_for(&check("recording", "changed"), dir).as_deref(),
            Some("rust-anything record port")
        );
        assert_eq!(
            fix_for(&check("behavior", "no recorded cases: run"), dir).as_deref(),
            Some("rust-anything record port")
        );
        assert!(fix_for(&check("fmt", ""), dir).is_some_and(|f| f.starts_with("cargo fmt")));
        assert_eq!(fix_for(&check("coverage", ""), dir), None);

        let mut missing_cmd = std::process::Command::new("cargo");
        missing_cmd.env_remove("PATH");
        let missing_err = crate::process::spawn_error(
            &missing_cmd,
            "cargo build",
            std::io::Error::new(std::io::ErrorKind::NotFound, "not found"),
        );
        let missing_fix = spawn_fix(&missing_err).unwrap();
        assert!(
            !missing_fix.contains("fix the compile error above"),
            "{missing_fix}"
        );
        assert!(missing_fix.contains("cargo"), "{missing_fix}");
        assert!(
            missing_fix.contains("rust-anything doctor"),
            "{missing_fix}"
        );
        assert!(!missing_fix.contains('\n'), "{missing_fix}");

        let denied_err = crate::process::spawn_error(
            &std::process::Command::new("cargo"),
            "cargo build",
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied"),
        );
        let spawn_hint = spawn_fix(&denied_err).unwrap();
        assert!(
            !spawn_hint.contains("fix the compile error above"),
            "{spawn_hint}"
        );
        assert!(spawn_hint.contains("rust-anything doctor"), "{spawn_hint}");

        let compile_err =
            anyhow::anyhow!("cargo build failed (exit status: 101)\n\"not found on PATH\"");
        assert!(spawn_fix(&compile_err).is_none());
        let compile_fix = fix_for(&check("build", &compile_err.to_string()), dir).unwrap();
        assert!(
            compile_fix.starts_with("fix the compile error above"),
            "{compile_fix}"
        );
    }

    #[test]
    fn requires_safety_comments() {
        let dir = std::env::temp_dir().join(format!("rust-anything-gate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("a.rs"),
            "#![forbid(unsafe_code)]\n\
             // SAFETY: checked above\n\
             unsafe { x() }\n\
             let y = unsafe { z() };\n\
             // unsafe in a comment\n\
             let _err = Err(\"RangeError: unsafe input\".into());\n\
             let _raw = r#\"unsafe \"quoted\" string\"#;\n\
             let _with_real = \"a string with unsafe\"; unsafe { real_work() };\n",
        )
        .unwrap();
        let (unexplained, explained) = unsafe_blocks(&dir);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(explained, 1);
        assert_eq!(unexplained.len(), 2);
        assert!(unexplained[0].ends_with("a.rs:4"), "{}", unexplained[0]);
        assert!(unexplained[1].ends_with("a.rs:8"), "{}", unexplained[1]);
    }

    #[test]
    fn strip_code_removes_strings_chars_and_comments() {
        assert_eq!(super::strip_code("// unsafe comment").trim(), "");
        assert_eq!(
            super::strip_code("let s = \"unsafe inside string\";"),
            "let s =  ;"
        );
        assert_eq!(
            super::strip_code("let s = r#\"unsafe in raw\"#;"),
            "let s =  ;"
        );
        assert_eq!(
            super::strip_code("let s = \"unsafe\"; unsafe { }"),
            "let s =  ; unsafe { }"
        );
        assert_eq!(
            super::strip_code("let s = \"http://example.com\"; unsafe { }"),
            "let s =  ; unsafe { }"
        );
    }

    #[test]
    fn missing_port_source_fails_the_ported_check() {
        let dir = crate::app::temp_dir("gate-port").unwrap();
        let check = super::ported_check(&dir);
        std::fs::remove_dir(dir).unwrap();
        assert!(!check.passed);
        assert!(check.detail.contains("cannot read"));
        assert!(check.detail.contains("port.rs"));
    }

    #[test]
    fn size_check_says_1_line_for_singular_source() {
        let dir = crate::app::temp_dir("gate-size").unwrap();
        let src_dir = dir.join("src");
        std::fs::create_dir_all(&src_dir).unwrap();
        std::fs::write(src_dir.join("add.ts"), "export const add = 1;\n").unwrap();
        std::fs::write(src_dir.join("port.rs"), "pub fn add() -> i32 { 1 }\n").unwrap();
        let manifest: crate::bridge::Manifest = serde_json::from_value(serde_json::json!({
            "schema": crate::bridge::SCHEMA,
            "root": dir,
            "unit": "src",
            "crate_name": "demo",
            "kind": "napi",
            "exports": [],
        }))
        .unwrap();
        let check = super::size_check(&dir, &manifest).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
        assert!(
            check
                .detail
                .contains("1 line of Rust for 1 line of TypeScript/JavaScript"),
            "{}",
            check.detail
        );
    }
}
