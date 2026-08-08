//! The date strings a camera writes.
//!
//! `parse_wall_clock` is the funnel every date in the tool passes through: EXIF
//! `DateTimeOriginal`, the `QuickTime` creation date, and the three XMP
//! properties all end up here. It hands its string to `chrono` under five
//! patterns in turn, and `chrono`'s parser is the thing under test as much as
//! the pattern list is — a strftime parser is a state machine over bytes, and
//! this one is being fed bytes from strangers.
//!
//! Takes `&str` rather than `&[u8]` because that is the real interface: the
//! callers hand it a `String` that `nom-exif` or `quick-xml` already decoded.
//! `Arbitrary` builds a `&str` from the input's longest valid UTF-8 prefix, so
//! multi-byte characters still reach the parser — which is the input that would
//! break any byte-offset slicing done underneath.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let Some((naive, offset)) = mmm::fuzz::parse_wall_clock(text) else {
        return;
    };

    // A parse that succeeded has to have produced an offset `chrono` will accept
    // back. The organiser attaches this offset to the naive reading to decide
    // which day a photograph belongs to (see `timezone::attach_offset`), and an
    // offset outside the range `FixedOffset` is defined over would make that
    // attachment fail on a value we just told the caller was good.
    if let Some(offset) = offset {
        let seconds = chrono::Offset::fix(&offset).local_minus_utc();
        assert!(
            seconds.abs() < 86_400,
            "parsed an offset of {seconds}s from {text:?}"
        );
    }

    // The date is going to be formatted into a directory name and a filename.
    // Whatever came out of the parser must survive that round trip, because the
    // path is what the user is left holding.
    let _ = naive.format("%Y-%m-%d").to_string();
});
