use std::env;
use std::path::PathBuf;

fn main() {
    // Only generate bindings for macOS
    if cfg!(target_os = "macos") {
        // Tell cargo to invalidate the built crate whenever the wrapper changes
        println!("cargo:rerun-if-changed=wrapper.h");

        // The bindgen::Builder is the main entry point
        // to bindgen, and lets you build up options for
        // the resulting bindings.
        let bindings = bindgen::Builder::default()
            // The input header we would like to generate
            // bindings for.
            .header("wrapper.h")
            // Tell bindgen to generate bindings for mach/thread_act.h
            .clang_arg("-F/System/Library/Frameworks")
            .allowlist_file("mach/thread_act.h")
            // Tell cargo to invalidate the built crate whenever any of the
            // included header files changed.
            .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
            // Finish the builder and generate the bindings.
            .generate()
            // Unwrap the Result and panic on failure.
            .expect("Unable to generate bindings");

        // Write the bindings to the $OUT_DIR/bindings.rs file.
        let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
        bindings
            .write_to_file(out_path.join("bindings.rs"))
            .expect("Couldn't write bindings!");
    }
} 