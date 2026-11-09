//! Records the running version: the package's, and the commit it is built from, such as `0.1.5+fb8d1c55`,
//! marked `-dirty` when the sources differ from that commit. Outside a git checkout, the package's version
//! alone.

use std::process::Command;

/// What the binary is built from: a change here, committed or not, changes the binary.
const SOURCES: [&str; 6] = [
    "src",
    "web/src",
    "web/site.json",
    "Cargo.toml",
    "Cargo.lock",
    "build.rs",
];

fn main() {
    let git = |args: &[&str]| {
        Command::new("git")
            .args(args)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .and_then(|output| String::from_utf8(output.stdout).ok())
            .map(|text| text.trim().to_owned())
    };
    // Rebuilt when HEAD moves (a checkout changes HEAD, a commit the branch it names), and when a source changes,
    // so the dirty mark follows the tree. Not on the index, which editors' git integrations refresh constantly.
    let mut watched = vec!["HEAD".to_owned(), "packed-refs".to_owned()];
    watched.extend(git(&["symbolic-ref", "-q", "HEAD"]).filter(|head| !head.is_empty()));
    for path in watched {
        if let Some(path) = git(&["rev-parse", "--git-path", &path]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    for path in SOURCES {
        println!("cargo:rerun-if-changed={path}");
    }
    let version = env!("CARGO_PKG_VERSION");
    let commit = git(&["rev-parse", "--short=8", "HEAD"]).filter(|commit| !commit.is_empty());
    let version = match commit {
        Some(commit) => {
            // Untracked sources count, as a new module would be built in; ignored files do not. No optional
            // locks, so the build never rewrites the index.
            let status = [
                &["--no-optional-locks", "status", "--porcelain", "--"][..],
                &SOURCES,
            ]
            .concat();
            let dirty = git(&status).is_none_or(|changes| !changes.is_empty());
            format!("{version}+{commit}{}", if dirty { "-dirty" } else { "" })
        }
        None => version.to_owned(),
    };
    println!("cargo:rustc-env=PUNTOHUNTER_VERSION={version}");
}
