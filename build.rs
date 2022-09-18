use std::{
    env,
    path::PathBuf,
};

fn main() {
    pkg_config::probe_library("libfreeipmi").unwrap();
    pkg_config::probe_library("libipmimonitoring").unwrap();

    println!("cargo:rerun-if-changed=wrapper.h");

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks))
        .allowlist_function("^ipmi_(cmd|completion_code|ctx|monitoring)_.*")
        .allowlist_type("^ipmi_monitoring_.*")
        .allowlist_var("^IPMI_(CMD|COMP_CODE|FLAGS|NET_FN|PRIVILEGE_LEVEL)_.*")
        .generate()
        .expect("Failed to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("bindings.rs"))
        .expect("Failed to write bindings");
}
