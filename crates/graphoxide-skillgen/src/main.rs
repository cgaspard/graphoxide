use graphoxide_skillgen::{check_on_disk, platforms, render_all, write_artifacts};
use std::path::PathBuf;

fn usage() -> &'static str {
    "Usage: graphoxide-skillgen [--platform HOST] [--root PATH] [--check] [--audit-coverage] [--schema-singleton]"
}

fn main() {
    if let Err(error) = run(std::env::args().skip(1)) {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

fn run(args: impl IntoIterator<Item = String>) -> Result<(), String> {
    let mut only = None;
    let mut root = PathBuf::from(".");
    let mut check = false;
    let mut audit = false;
    let mut schema = false;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--platform" => {
                only = Some(
                    args.next()
                        .ok_or_else(|| "--platform requires a host name".to_owned())?,
                );
            }
            "--root" => {
                root = PathBuf::from(
                    args.next()
                        .ok_or_else(|| "--root requires a path".to_owned())?,
                );
            }
            "--check" => check = true,
            "--audit-coverage" => audit = true,
            "--schema-singleton" => schema = true,
            "-h" | "--help" => {
                println!("{}", usage());
                return Ok(());
            }
            other => return Err(format!("unknown argument {other:?}\n{}", usage())),
        }
    }

    let platforms = platforms();
    let artifacts = render_all(&platforms, only.as_deref())?;

    if audit {
        let selected = only
            .as_deref()
            .map(|key| platforms.get(key).into_iter().collect::<Vec<_>>())
            .unwrap_or_else(|| platforms.values().collect());
        let problems = selected
            .into_iter()
            .flat_map(graphoxide_skillgen::audit_coverage)
            .collect::<Vec<_>>();
        if !problems.is_empty() {
            return Err(problems.join("\n"));
        }
    }
    if schema {
        let problems = graphoxide_skillgen::schema_singleton(&platforms);
        if !problems.is_empty() {
            return Err(problems.join("\n"));
        }
    }
    if check {
        let problems = check_on_disk(&root, &artifacts);
        if !problems.is_empty() {
            return Err(problems.join("\n"));
        }
    } else if !audit && !schema {
        write_artifacts(&root, &artifacts).map_err(|error| error.to_string())?;
    }
    Ok(())
}
