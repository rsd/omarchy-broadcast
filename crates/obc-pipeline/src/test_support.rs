//! Helpers shared by the unit tests.
//!
//! A test never passes without running: a prerequisite it lacks fails it
//! with a message naming what is missing. Tests that need the GStreamer
//! CUDA elements are `#[ignore]` and named `gpu_*` (`docs/testing.md`).

/// The fake module chain shared with gst-obc's `obcmodule` tests.
#[path = "../../gst-obc/tests/support/fake_chain.rs"]
pub(crate) mod fake_chain;

/// Initialises GStreamer and panics unless every element in `factories` is
/// installed, naming the missing ones and where they come from.
pub(crate) fn require_elements(factories: &[&str]) {
    gst::init().expect("gst::init");
    let missing: Vec<&str> = factories
        .iter()
        .copied()
        .filter(|f| gst::ElementFactory::find(f).is_none())
        .collect();
    assert!(missing.is_empty(), "{}", missing_elements(&missing));
}

/// Why a test cannot run without the `missing` GStreamer elements.
fn missing_elements(missing: &[&str]) -> String {
    let cuda = missing.iter().any(|f| f.starts_with("cuda"));
    format!(
        "GStreamer element(s) not installed: {}. {}Check with `gst-inspect-1.0 <element>`.",
        missing.join(", "),
        if cuda {
            "The cuda* elements come from the nvcodec plugin (gst-plugins-bad) and register \
             only where an NVIDIA GPU and its driver are present; the others from \
             GStreamer core, gst-plugins-base and gst-plugins-good. "
        } else {
            "They come from GStreamer core, gst-plugins-base and gst-plugins-good, which the \
             tests require. "
        }
    )
}

mod tests {
    use super::*;

    #[test]
    fn missing_elements_are_named_with_their_plugin() {
        let cuda = missing_elements(&["cudaupload", "fakesink"]);
        assert!(cuda.contains("cudaupload, fakesink"), "{cuda}");
        assert!(cuda.contains("nvcodec"), "{cuda}");
        let core = missing_elements(&["videocrop"]);
        assert!(
            core.contains("videocrop") && !core.contains("nvcodec"),
            "{core}"
        );
    }

    #[test]
    #[should_panic(expected = "no-such-element")]
    fn a_missing_element_fails_the_test() {
        require_elements(&["fakesink", "no-such-element"]);
    }
}
