//! Puts the web build (made by `scripts/build-web.sh` into `web/dist`, or wherever
//! `ITMO_LABELS_WEB_DIST` points) into `$OUT_DIR/web` for the server to embed. Without one the
//! files are left empty and the server says so, so a plain `cargo build` still works.

use std::{env, fs, path::PathBuf};

const BUILT: [&str; 2] = ["itmo-labels.js", "itmo-labels_bg.wasm"];

fn main() {
    println!("cargo::rerun-if-env-changed=ITMO_LABELS_WEB_DIST");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("web");
    fs::create_dir_all(&out).unwrap();
    // The web build itself has no server and needs none of this.
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        return;
    }
    let index = manifest.join("web/index.html");
    println!("cargo::rerun-if-changed={}", index.display());
    fs::copy(&index, out.join("index.html")).unwrap();

    let dist = env::var_os("ITMO_LABELS_WEB_DIST").map_or_else(|| manifest.join("web/dist"), PathBuf::from);
    let complete = BUILT.iter().all(|f| dist.join(f).is_file());
    for file in BUILT {
        let from = dist.join(file);
        println!("cargo::rerun-if-changed={}", from.display());
        if complete {
            fs::copy(&from, out.join(file)).unwrap();
        } else {
            fs::write(out.join(file), b"").unwrap();
        }
    }
    if !complete {
        println!("cargo::warning=no web build in {}: server mode will serve a placeholder (run scripts/build-web.sh)", dist.display());
    }
}
