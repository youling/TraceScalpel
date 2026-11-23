fn main() {
    for (binary, level) in [
        ("tracescalpel", "asInvoker"),
        ("tracescalpel-capture", "requireAdministrator"),
    ] {
        println!("cargo:rustc-link-arg-bin={binary}=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg-bin={binary}=/MANIFESTUAC:level='{level}' uiAccess='false'");
    }
}
