//! The ISO 6709 location string a video container carries.
//!
//! This is the parser in the tool with the most hand-rolled byte handling:
//! it searches for sign characters, slices the string at the offsets it finds,
//! and hands both halves to `f64::from_str`. Two things are being looked for.
//!
//! **A slice at a non-boundary.** The searches are for `+` and `-`, which are
//! ASCII, and an ASCII byte can never occur inside a multi-byte UTF-8 sequence —
//! so the slices ought to be at character boundaries by construction. "Ought to"
//! is what a fuzzer is for; a panic here is an index-out-of-bounds in a parser
//! reading somebody's holiday video.
//!
//! **A coordinate that is not one.** `f64::from_str` accepts `NaN`, `inf` and
//! `-inf`, and the value goes straight to the reverse geocoder, which does not
//! reject them — it returns the first record in its k-d tree. The assertion
//! below is what holds that shut.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let Some((lat, lon)) = mmm::fuzz::parse_iso6709(text) else {
        return;
    };

    // Anything this returns is handed to `geocoder::GeoLookup::lookup`, which
    // names a file after the nearest city. A non-finite or off-planet coordinate
    // does not fail there — it silently resolves to whichever record the k-d
    // tree happens to reach first, and the user gets a photograph filed under a
    // country they have never been to. Refusing the coordinate is the only
    // outcome that leaves them with the truth (no location) rather than a
    // fabricated one.
    assert!(
        lat.is_finite() && lon.is_finite(),
        "non-finite coordinate ({lat}, {lon}) from {text:?}"
    );
    assert!(
        (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon),
        "off-planet coordinate ({lat}, {lon}) from {text:?}"
    );
});
