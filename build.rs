//! Inject the current git SHA into the build so `cmmd_build_info` can
//! publish it on the Prometheus endpoint. Best-effort — empty string if
//! we're not inside a git checkout.

use std::process::Command;

fn main() {
    let sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_default();
    println!("cargo:rustc-env=CMMD_GIT_SHA={}", sha);

    // Force a rebuild when HEAD moves so the SHA stays current.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs");
}
