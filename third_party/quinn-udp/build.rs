// veil: upstream は cfg_aliases! マクロで同じ cfg を定義している。path 依存では依存クレートの
// lint が cap されず、マクロ内部の警告（semicolon_in_expressions_from_macros）が出続けるため、
// 同じ別名を build スクリプトで直接定義する（意味は upstream と同一）。
use std::env;

fn main() {
    let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let family = env::var("CARGO_CFG_TARGET_FAMILY").unwrap_or_default();
    let fast_apple = env::var_os("CARGO_FEATURE_FAST_APPLE_DATAPATH").is_some();

    let apple = matches!(os.as_str(), "macos" | "ios" | "tvos" | "visionos");
    let bsd = matches!(os.as_str(), "freebsd" | "openbsd" | "netbsd");
    let solarish = matches!(os.as_str(), "solaris" | "illumos");
    let wasm_browser = family.split(',').any(|f| f == "wasm") && os == "unknown";

    for (name, on) in [
        ("apple", apple),
        ("bsd", bsd),
        ("solarish", solarish),
        ("apple_fast", apple && fast_apple),
        ("apple_slow", apple && !fast_apple),
        ("wasm_browser", wasm_browser),
    ] {
        println!("cargo:rustc-check-cfg=cfg({name})");
        if on {
            println!("cargo:rustc-cfg={name}");
        }
    }
}
