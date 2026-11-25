fn main() {
    println!("cargo:rustc-link-arg-cdylib=/DELAYLOAD:vanta-probe-delayed.dll");
    println!("cargo:rustc-link-lib=delayimp");
}
