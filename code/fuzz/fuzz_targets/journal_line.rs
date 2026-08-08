//! One line of a run journal.
//!
//! The journal is the only thing standing between an interrupted run and an
//! unrecoverable library, and `mmm undo` reads it back from a disk that may have
//! been power-cycled mid-write. `Journal::read` already promises to survive a
//! truncated final line; this target is the rest of that promise — that *any*
//! byte sequence at all produces an error rather than a panic, because a panic
//! in the reader is an undo that cannot run.
//!
//! Both line kinds are tried against the same input. The header and the entries
//! go through the same `parse_line`, and which of the two a given byte string
//! resembles is not something a fuzzer should have to guess.
//!
//! The round trip is the second half. Undo acts on what it reads: if an entry
//! can be parsed but does not survive being written back out and read again, the
//! journal format has a value it can express and not preserve — and the file
//! that entry names is the one that would not be restored.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(header) = mmm::fuzz::journal_header_line(data) {
        let written = serde_json::to_vec(&header).expect("a parsed header must re-serialise");
        let reread = mmm::fuzz::journal_header_line(&written).expect("a written header must parse");
        assert_eq!(header, reread, "header did not survive a round trip");
    }

    if let Ok(entry) = mmm::fuzz::journal_entry_line(data) {
        let written = serde_json::to_vec(&entry).expect("a parsed entry must re-serialise");
        let reread = mmm::fuzz::journal_entry_line(&written).expect("a written entry must parse");
        assert_eq!(entry, reread, "entry did not survive a round trip");
    }
});
