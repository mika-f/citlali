fn main() {
    // `keep=none` in the save options needs libvips 8.15.
    if let Err(e) = pkg_config::Config::new().atleast_version("8.15").probe("vips") {
        panic!("libvips >= 8.15 not found via pkg-config: {e}");
    }
}
