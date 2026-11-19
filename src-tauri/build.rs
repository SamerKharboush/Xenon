fn main() {
    // On macOS release builds for the universal target, rustc links with
    // `-nodefaultlibs`, so clang's compiler-rt (libclang_rt.osx.a) is not
    // linked. whisper-rs-sys (ggml-metal-device.m.o) uses Objective-C
    // `@available` checks that reference `___isPlatformVersionAtLeast`,
    // defined in libclang_rt.osx.a. Without it, the aarch64 release link of
    // the `xenon` bin fails with "Undefined symbols for architecture arm64".
    // Debug host builds work because rustc links default libs there.
    //
    // `rustc-link-lib` only propagates to the lib target (xenon_lib), not to
    // the bin's final link, so we also pass the archive directly to the bin
    // linker via `rustc-link-arg-bins` (appended after the rlibs, so the
    // static symbol resolves).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        if let Ok(clang) = std::process::Command::new("xcrun")
            .args(["--find", "clang"])
            .output()
        {
            let clang = String::from_utf8_lossy(&clang.stdout).trim().to_string();
            if !clang.is_empty() {
                if let Ok(res) = std::process::Command::new(&clang)
                    .arg("--print-resource-dir")
                    .output()
                {
                    let res = String::from_utf8_lossy(&res.stdout).trim().to_string();
                    if !res.is_empty() {
                        let lib_dir = format!("{}/lib/darwin", res);
                        let archive = format!("{}/libclang_rt.osx.a", lib_dir);
                        println!("cargo:rustc-link-search=native={}", lib_dir);
                        println!("cargo:rustc-link-lib=static=clang_rt.osx");
                        println!("cargo:rustc-link-arg-bins={}", archive);
                    } else {
                        println!("cargo:warning=clang --print-resource-dir returned empty");
                    }
                } else {
                    println!("cargo:warning=failed to run clang --print-resource-dir");
                }
            } else {
                println!("cargo:warning=xcrun --find clang returned empty");
            }
        } else {
            println!("cargo:warning=failed to run xcrun --find clang");
        }
    }
    tauri_build::build()
}
