fn main() {
    if std::env::var_os("CARGO_FEATURE_WEB_UI").is_some() {
        println!("cargo:rerun-if-changed=../../web/dist");
    }
}
