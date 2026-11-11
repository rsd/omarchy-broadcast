//! The library searches `./` for the feature libraries it opens by their
//! unversioned names, as the SDK's core library does: a run path whose
//! first entry is `./`, written as `DT_RPATH` (`--disable-new-dtags`) like
//! the SDK's, so it is searched before `LD_LIBRARY_PATH`. The tests start
//! their processes with a working directory that holds no feature library,
//! so the library search path is what finds them, as in the audio worker.

fn main() {
    println!("cargo:rustc-cdylib-link-arg=-Wl,--disable-new-dtags");
    println!("cargo:rustc-cdylib-link-arg=-Wl,-rpath,./");
}
