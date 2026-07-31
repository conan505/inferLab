use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

fn main() {
    println!("cargo:rerun-if-changed=cpp/inferlab_runtime.cpp");
    println!("cargo:rerun-if-changed=cpp/inferlab_runtime.h");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo sets OUT_DIR"));
    let source = Path::new("cpp/inferlab_runtime.cpp");
    let object = out_dir.join("inferlab_runtime.o");
    let archive = out_dir.join("libinferlab_runtime.a");
    let compiler = env::var("CXX").unwrap_or_else(|_| "c++".to_owned());

    run(
        Command::new(&compiler)
            .arg("-std=c++20")
            .arg("-O2")
            .arg("-Wall")
            .arg("-Wextra")
            .arg("-Wpedantic")
            .arg("-Werror")
            .arg("-c")
            .arg(source)
            .arg("-o")
            .arg(&object),
        "compile the C++ runtime",
    );
    run(
        Command::new("ar").arg("crs").arg(&archive).arg(&object),
        "archive the C++ runtime",
    );

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=inferlab_runtime");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        println!("cargo:rustc-link-lib=dylib=c++");
    } else {
        println!("cargo:rustc-link-lib=dylib=stdc++");
    }
}

fn run(command: &mut Command, action: &str) {
    let status = command
        .status()
        .unwrap_or_else(|error| panic!("failed to {action}: {error}"));
    assert!(status.success(), "failed to {action}: exit status {status}");
}
