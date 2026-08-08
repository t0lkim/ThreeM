//! An XMP sidecar, as bytes off the disk.
//!
//! The largest surface of the four. A sidecar is RDF/XML written by somebody
//! else's exporter, and `xmp::parse` drives a streaming pull parser over it —
//! namespace resolution, attribute normalisation, entity unescaping and a
//! decoder that has to cope with whatever encoding the packet declares. Every
//! one of those is a place a malformed document can go wrong, and the module's
//! stated contract is that *no* malformed document produces anything worse than
//! `None` and a line in the log.
//!
//! Bytes rather than `&str`, because that is what the file is: an XMP packet
//! declares its own encoding and the parser is entitled to be handed rubbish
//! that is not UTF-8 at all.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some(date) = mmm::fuzz::xmp_date(data) else {
        return;
    };

    // `has_time_of_day = false` is the flag that stops a day-precision value
    // beating a lower-ranked property that knows the hour. It is only sound
    // because such a value is filed at midnight — an invented hour the caller
    // has been told about. A value flagged as date-only that carries a real time
    // of day would be a silent lie to `xmp::better`.
    if !date.has_time_of_day {
        assert_eq!(
            date.naive.time(),
            chrono::NaiveTime::MIN,
            "a date-only value carrying a time of day: {date:?}"
        );
    }

    if let Some(offset) = date.offset {
        let seconds = chrono::Offset::fix(&offset).local_minus_utc();
        assert!(
            seconds.abs() < 86_400,
            "parsed an offset of {seconds}s from a sidecar"
        );
    }
});
