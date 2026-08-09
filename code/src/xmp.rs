//! Reading a date out of an XMP sidecar.
//!
//! [`crate::sidecar`] treats a sidecar as a passenger: a file bound to a
//! photograph by name, which has to arrive at the destination still bound to it.
//! This module is the other half — for one of the three formats, a sidecar is
//! also a *witness*. An `.xmp` written beside a RAW file records when the
//! photograph was taken, and for the largest family of files this tool handles
//! that is the only place the date can be read at all: `nom-exif` recognises
//! four containers, and no TIFF-based RAW is one of them (see
//! `docs/reference/format-support.md`). A darktable or Lightroom user's CR2
//! library currently files entirely under filesystem timestamps while the answer
//! sits in a text file next to every single frame.
//!
//! ## What is read, and what is not
//!
//! Three properties, and only when the media file itself yielded no date worth
//! having. XMP is a large specification and an RDF serialisation of it can carry
//! hundreds of properties, structures, language alternatives and a full edit
//! history; none of that is wanted here. The whole question is "when was this
//! taken", it has three spellings in the wild, and a streaming pull parser
//! answers it in one pass over the file.
//!
//! ## Both serialisations, because exporters differ
//!
//! RDF/XML lets the same property be written as an attribute or as a child
//! element, and the choice is the exporter's:
//!
//! ```xml
//! <rdf:Description xmp:CreateDate="2024-03-15T23:30:00+08:00"/>
//!
//! <rdf:Description>
//!   <xmp:CreateDate>2024-03-15T23:30:00+08:00</xmp:CreateDate>
//! </rdf:Description>
//! ```
//!
//! Adobe writes the first, darktable the second, and a file that has been
//! through both tools may hold a mixture. Reading one form only would work
//! perfectly against whichever fixture happened to be written first and fail
//! silently against half the libraries in the world.

use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::{NsReader, XmlVersion};
use tracing::{debug, warn};

/// The property an XMP date was read from.
///
/// Carried on the result so the caller can log *which* of the three answered,
/// and so [`Property::rank`] has somewhere to live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Property {
    /// `exif:DateTimeOriginal` — the EXIF shutter time, relocated into XMP by
    /// whatever ingested the file.
    ExifDateTimeOriginal,
    /// `photoshop:DateCreated` — the IPTC "date created" of the intellectual
    /// content, which for a photograph is also the shutter time. Frequently
    /// written to day precision and no finer.
    PhotoshopDateCreated,
    /// `xmp:CreateDate` — when the *resource* was created.
    XmpCreateDate,
}

impl Property {
    /// How near this property is to the moment the shutter fired. Lower wins.
    ///
    /// **This is the reverse of the order the three are usually listed in**, and
    /// the reason is the same one that makes `OffsetTimeOriginal` outrank
    /// `OffsetTime` in [`crate::metadata`]: prefer the tag that is defined as
    /// being about the exposure over one that is defined as being about the
    /// file.
    ///
    /// `exif:DateTimeOriginal` *is* the shutter time by definition.
    /// `photoshop:DateCreated` is the creation of the content, which for a
    /// photograph is the same moment described from one step further away.
    /// `xmp:CreateDate` is the creation of the resource — and for a JPEG
    /// exported from a RAW five years later, the resource was created five years
    /// later. It is last because it is the only one of the three that can
    /// legitimately hold a date that is not the photograph's.
    const fn rank(self) -> u8 {
        match self {
            Self::ExifDateTimeOriginal => 0,
            Self::PhotoshopDateCreated => 1,
            Self::XmpCreateDate => 2,
        }
    }

    /// The spelling, for logs.
    pub const fn name(self) -> &'static str {
        match self {
            Self::ExifDateTimeOriginal => "exif:DateTimeOriginal",
            Self::PhotoshopDateCreated => "photoshop:DateCreated",
            Self::XmpCreateDate => "xmp:CreateDate",
        }
    }
}

/// One of the three properties, in the two ways it can be recognised.
///
/// The namespace URI is the identity of a property; the prefix is only its
/// customary spelling and a writer is free to bind `http://ns.adobe.com/xap/1.0/`
/// to any prefix it likes. Matching on the URI is therefore the correct rule —
/// and matching on the prefix as well is what keeps a sidecar readable when its
/// `xmlns` declarations have been lost, which is exactly the sort of damage that
/// leaves the rest of the file perfectly legible.
struct Known {
    property: Property,
    prefix: &'static [u8],
    namespace: &'static [u8],
    local: &'static [u8],
}

/// The three, with both of their identities.
///
/// The local names are distinct across the three namespaces, so a lookup by
/// local name has at most one candidate and the namespace check that follows is
/// a confirmation rather than a disambiguation.
const KNOWN: &[Known] = &[
    Known {
        property: Property::ExifDateTimeOriginal,
        prefix: b"exif",
        namespace: b"http://ns.adobe.com/exif/1.0/",
        local: b"DateTimeOriginal",
    },
    Known {
        property: Property::PhotoshopDateCreated,
        prefix: b"photoshop",
        namespace: b"http://ns.adobe.com/photoshop/1.0/",
        local: b"DateCreated",
    },
    Known {
        property: Property::XmpCreateDate,
        prefix: b"xmp",
        namespace: b"http://ns.adobe.com/xap/1.0/",
        local: b"CreateDate",
    },
];

/// A date read out of a sidecar, and how much the sidecar actually said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SidecarDate {
    /// The wall clock the sidecar recorded.
    pub naive: NaiveDateTime,
    /// The offset it recorded alongside, when it recorded one. `None` leaves the
    /// reading naive, to be resolved by the run's [`crate::timezone`] policy
    /// exactly as a bare EXIF `DateTimeOriginal` is.
    pub offset: Option<FixedOffset>,
    /// Which property answered.
    pub property: Property,
    /// Whether the value carried a time of day, or only a date.
    ///
    /// A date-only value is filed at midnight, which is a true day and an
    /// invented hour. That is worth having — the directory is the user-visible
    /// half — but it must not beat a value from a lower-ranked property that
    /// knows the hour. See [`better`].
    pub has_time_of_day: bool,
}

/// The largest sidecar this will read.
///
/// A real XMP sidecar is a few kilobytes: an `rdf:Description` with a few dozen
/// properties. Ten megabytes is three orders of magnitude past anything a
/// camera or an editor writes, so the cap refuses only files that are not
/// sidecars — a crafted one aimed at memory exhaustion, or something that
/// acquired the extension by accident.
///
/// Chosen as a round number rather than measured against a corpus, and stated
/// that way: the point is a ceiling far above legitimate use, not a tight
/// bound.
const MAX_SIDECAR_BYTES: u64 = 10 * 1024 * 1024;

/// The date an XMP sidecar records, if it records one this can use.
///
/// Returns `None` for every kind of failure, and each of them is a line in the
/// log rather than an error: the caller has a filesystem timestamp to fall back
/// on, and a photo library is exactly the place where one unreadable text file
/// must not stop the other forty thousand from being organised.
pub fn read_date(path: &Path) -> Option<SidecarDate> {
    let file = File::open(path)
        .map_err(|e| warn!(sidecar = %path.display(), error = %e, "could not open sidecar"))
        .ok()?;

    // Refused before a byte is parsed. The parse itself streams — `NsReader`
    // over a `BufReader`, not a `read_to_string` — but streaming bounds how much
    // is read at a time, not how much a *single* element can allocate: one
    // unclosed tag holding a gigabyte of text is one `Vec` growing to a
    // gigabyte. A sidecar is a few kilobytes of XML written by a photo editor,
    // so a file over the cap is not a sidecar this can use however it is
    // parsed, and the cheapest correct answer is not to start.
    match file.metadata() {
        Ok(meta) if meta.len() > MAX_SIDECAR_BYTES => {
            warn!(
                sidecar = %path.display(),
                bytes = meta.len(),
                limit = MAX_SIDECAR_BYTES,
                "sidecar is too large to be a sidecar; ignoring it"
            );
            return None;
        }
        Ok(_) => {}
        // Unreadable metadata is not a reason to refuse the file — the parse
        // below will fail on its own terms and log why, and treating a stat
        // failure as "too large" would report the wrong cause.
        Err(e) => {
            debug!(sidecar = %path.display(), error = %e, "could not size the sidecar");
        }
    }

    let found = parse(BufReader::new(file), path);

    if let Some(date) = &found {
        debug!(
            sidecar = %path.display(),
            property = date.property.name(),
            wall_clock = %date.naive,
            "read a date from an XMP sidecar"
        );
    } else {
        debug!(sidecar = %path.display(), "no usable date in the sidecar");
    }
    found
}

/// The parse itself, over any reader — split out so the unit tests below can
/// drive it from a string without touching the filesystem.
///
/// `origin` names the source in the log lines only.
///
/// Visible to the crate for the same reason it was split out at all, one step
/// further: [`crate::fuzz`] drives it from a byte slice.
pub(crate) fn parse<R: std::io::BufRead>(source: R, origin: &Path) -> Option<SidecarDate> {
    let mut reader = NsReader::from_reader(source);
    reader.config_mut().trim_text(true);

    let mut buf = Vec::new();
    let mut best: Option<SidecarDate> = None;
    // The property whose element we are inside, for the element serialisation.
    // Cleared by every start and every end, so a nested element cannot make its
    // own text look like its parent's value.
    let mut open: Option<Property> = None;

    loop {
        // `read_event_into` rather than `read_resolved_event_into`: the
        // namespace bindings have to be consulted for the *attributes* too, and
        // resolving them needs a borrow of the reader that the resolved-event
        // form would still be holding.
        let event = match reader.read_event_into(&mut buf) {
            Ok(event) => event,
            Err(e) => {
                // The whole of the malformed-sidecar contract, in one arm. Note
                // that whatever was read before the damage is kept: a file
                // truncated mid-history still had a valid `xmp:CreateDate` in
                // its first hundred bytes, and discarding that would be
                // throwing away an answer we already have.
                warn!(
                    sidecar = %origin.display(),
                    error = %e,
                    "sidecar is not well-formed XML; reading no further"
                );
                break;
            }
        };

        match event {
            Event::Eof => break,
            Event::Start(ref element) | Event::Empty(ref element) => {
                for attribute in element.attributes().flatten() {
                    let (namespace, local) = reader.resolver().resolve_attribute(attribute.key);
                    let Some(property) = property_of(&namespace, local.as_ref()) else {
                        continue;
                    };
                    if let Ok(value) = attribute
                        .decoded_and_normalized_value(XmlVersion::Implicit1_0, reader.decoder())
                    {
                        consider(&mut best, property, value.trim());
                    }
                }

                let (namespace, local) = reader.resolver().resolve_element(element.name());
                // `Empty` cannot have text, so an element-form match on one is
                // simply an empty value; clearing either way keeps the state
                // machine honest.
                open = matches!(event, Event::Start(_))
                    .then(|| property_of(&namespace, local.as_ref()))
                    .flatten();
            }
            Event::Text(text) => {
                if let Some(property) = open {
                    if let Ok(value) = text.decode() {
                        consider(&mut best, property, value.trim());
                    }
                }
            }
            Event::CData(data) => {
                if let Some(property) = open {
                    if let Ok(value) = std::str::from_utf8(&data) {
                        consider(&mut best, property, value.trim());
                    }
                }
            }
            Event::End(_) => open = None,
            _ => {}
        }

        buf.clear();
    }

    best
}

/// Which of the three properties a resolved name is, if it is one.
///
/// [`ResolveResult::Unbound`] — a bare `<CreateDate>` with no prefix and no
/// default namespace — is deliberately not matched. An unprefixed name in a file
/// that declares no default namespace is not an XMP property; treating it as one
/// would read a date out of any XML document that happened to use the word.
fn property_of(namespace: &ResolveResult<'_>, local: &[u8]) -> Option<Property> {
    let known = KNOWN.iter().find(|known| known.local == local)?;
    match namespace {
        ResolveResult::Bound(bound) if bound.as_ref() == known.namespace => Some(known.property),
        ResolveResult::Unknown(prefix) if prefix == known.prefix => Some(known.property),
        _ => None,
    }
}

/// Parse one property's value and keep it if it beats what we have.
fn consider(best: &mut Option<SidecarDate>, property: Property, value: &str) {
    let Some((naive, offset, has_time_of_day)) = parse_value(value) else {
        debug!(
            property = property.name(),
            value, "ignoring an XMP date that is not one"
        );
        return;
    };

    let candidate = SidecarDate {
        naive,
        offset,
        property,
        has_time_of_day,
    };

    if best.is_none_or(|held| better(&candidate, &held)) {
        *best = Some(candidate);
    }
}

/// Whether `candidate` is a better answer than `held`.
///
/// Time of day outranks property precedence, which is the one place this departs
/// from a plain ordering. An Adobe export routinely writes
/// `photoshop:DateCreated` to day precision beside an `xmp:CreateDate` carrying
/// the full timestamp, and strict precedence would take the higher-ranked
/// property and file the photograph at midnight — losing an hour that the file
/// in front of us plainly states. Between two values that both know the hour, or
/// two that both do not, rank decides.
fn better(candidate: &SidecarDate, held: &SidecarDate) -> bool {
    let key = |date: &SidecarDate| (u8::from(!date.has_time_of_day), date.property.rank());
    key(candidate) < key(held)
}

/// An XMP date value, in the forms the specification allows and exporters write.
///
/// Returns the wall clock, any offset stated alongside it, and whether the value
/// named a time of day at all.
///
/// **Precision below a day is refused.** XMP permits `2024` and `2024-03`, and
/// both are honest statements of an imprecise date — but this tool files into a
/// dated directory and names files after a timestamp, so accepting them means
/// inventing a day. `2024-03` filed under the 1st of March is a fabrication a
/// reader has no way to detect, where falling through to the filesystem
/// timestamp is at least reported as such. A date-only value is the boundary:
/// the day is true and only the hour is invented, and the hour is not what the
/// directory is named after.
fn parse_value(value: &str) -> Option<(NaiveDateTime, Option<FixedOffset>, bool)> {
    // The full form first — RFC 3339 covers the offset spellings, the `Z`
    // spelling that means UTC, and the fractional seconds some exporters write.
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Some((dt.naive_local(), Some(*dt.offset()), true));
    }

    // Then the spellings the media files themselves use. Shared with
    // [`crate::metadata`] rather than re-listed here: an `exif:DateTimeOriginal`
    // relocated into XMP is sometimes relocated verbatim, colons and all, and a
    // second copy of that list would be a second place to fix.
    if let Some((naive, offset)) = crate::metadata::parse_wall_clock(value) {
        return Some((naive, offset, true));
    }

    // A time of day with no seconds, which XMP allows and RFC 3339 does not.
    for pattern in ["%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(value, pattern) {
            return Some((naive, None, true));
        }
    }

    // A bare date. Midnight, and the caller is told the hour was not stated.
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Some((date.and_hms_opt(0, 0, 0)?, None, false));
    }

    None
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]
mod tests {
    use super::*;

    fn read(xml: &str) -> Option<SidecarDate> {
        parse(xml.as_bytes(), Path::new("<test>"))
    }

    /// Wrap property markup in the packet an exporter actually writes, so every
    /// test below runs against a realistic namespace scope rather than a bare
    /// fragment.
    fn packet(body: &str) -> String {
        format!(
            r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
          xmlns:xmp="http://ns.adobe.com/xap/1.0/"
          xmlns:exif="http://ns.adobe.com/exif/1.0/"
          xmlns:photoshop="http://ns.adobe.com/photoshop/1.0/">
{body}
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"#
        )
    }

    fn wall_clock(date: &SidecarDate) -> String {
        date.naive.format("%Y-%m-%d %H:%M:%S").to_string()
    }

    // -----------------------------------------------------------------
    // The two serialisations
    // -----------------------------------------------------------------

    /// Adobe's form: the property is an attribute of `rdf:Description`.
    #[test]
    fn the_attribute_serialisation_is_read() {
        let date = read(&packet(
            r#"<rdf:Description rdf:about="" xmp:CreateDate="2024-03-15T23:30:00+08:00"/>"#,
        ))
        .expect("an attribute-form date");

        assert_eq!(wall_clock(&date), "2024-03-15 23:30:00");
        assert_eq!(date.offset, FixedOffset::east_opt(8 * 3600));
        assert_eq!(date.property, Property::XmpCreateDate);
        assert!(date.has_time_of_day);
    }

    /// darktable's form: the property is a child element.
    #[test]
    fn the_element_serialisation_is_read() {
        let date = read(&packet(
            r"<rdf:Description rdf:about=''>
                <xmp:CreateDate>2024-03-15T23:30:00+08:00</xmp:CreateDate>
              </rdf:Description>",
        ))
        .expect("an element-form date");

        assert_eq!(wall_clock(&date), "2024-03-15 23:30:00");
        assert_eq!(date.offset, FixedOffset::east_opt(8 * 3600));
        assert_eq!(date.property, Property::XmpCreateDate);
    }

    /// A file that has been through both tools carries a mixture, and the
    /// ordinary precedence has to apply across the two forms.
    #[test]
    fn the_two_serialisations_are_read_from_the_same_file() {
        let date = read(&packet(
            r#"<rdf:Description rdf:about="" xmp:CreateDate="2020-01-01T09:00:00Z">
                <exif:DateTimeOriginal>2024-03-15T23:30:00+08:00</exif:DateTimeOriginal>
               </rdf:Description>"#,
        ))
        .expect("a date");

        assert_eq!(date.property, Property::ExifDateTimeOriginal);
        assert_eq!(wall_clock(&date), "2024-03-15 23:30:00");
    }

    // -----------------------------------------------------------------
    // Malformed input
    // -----------------------------------------------------------------

    /// The contract: a warning and a skip, never a failure. Asserted over
    /// several shapes of damage because they leave the parser in different
    /// places — one is not XML at all, one is truncated mid-tag, one closes an
    /// element that was never opened.
    #[test]
    fn a_malformed_sidecar_yields_nothing_and_does_not_panic() {
        for broken in [
            "this is not xml at all",
            "",
            "<x:xmpmeta><rdf:RDF><rdf:Description xmp:CreateDate=\"2024-",
            "</rdf:Description>",
            "<a><b></a></b>",
            "\u{feff}\u{0}\u{1}\u{2}not xml",
        ] {
            assert_eq!(read(broken), None, "{broken:?} must yield no date");
        }
    }

    /// Damage *after* a good value does not retract it. A history block
    /// truncated by a full disk is the common case, and the date at the top of
    /// the file was read correctly before the parser ever reached the damage.
    #[test]
    fn a_date_read_before_the_damage_is_kept() {
        let date = read(
            r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
                        xmlns:xmp="http://ns.adobe.com/xap/1.0/">
                 <rdf:Description xmp:CreateDate="2024-03-15T23:30:00+08:00"/>
                 <rdf:Description><xmp:Truncated"#,
        )
        .expect("the date that was readable");

        assert_eq!(wall_clock(&date), "2024-03-15 23:30:00");
    }

    /// A well-formed sidecar that simply says nothing about a date — an edit
    /// history and a rating, which is a perfectly ordinary `.xmp`.
    #[test]
    fn a_sidecar_with_no_date_property_yields_nothing() {
        assert_eq!(
            read(&packet(
                r#"<rdf:Description rdf:about="" xmp:Rating="4" xmp:Label="Green"/>"#
            )),
            None
        );
    }

    /// A property that is there and holds rubbish is not a date either, and must
    /// not take the place of one that is readable.
    #[test]
    fn an_unparseable_value_is_skipped_and_the_readable_one_wins() {
        let date = read(&packet(
            r#"<rdf:Description rdf:about=""
                 exif:DateTimeOriginal="the fourteenth of never"
                 xmp:CreateDate="2024-03-15T23:30:00+08:00"/>"#,
        ))
        .expect("the readable one");

        assert_eq!(date.property, Property::XmpCreateDate);
    }

    // -----------------------------------------------------------------
    // Which property answers
    // -----------------------------------------------------------------

    /// Precedence runs towards the shutter, which is the reverse of the order
    /// the three are usually listed in.
    #[test]
    fn the_property_nearest_the_shutter_wins() {
        let date = read(&packet(
            r#"<rdf:Description rdf:about=""
                 xmp:CreateDate="2020-01-01T09:00:00+00:00"
                 photoshop:DateCreated="2022-02-02T10:00:00+00:00"
                 exif:DateTimeOriginal="2024-03-15T23:30:00+08:00"/>"#,
        ))
        .expect("a date");

        assert_eq!(date.property, Property::ExifDateTimeOriginal);
        assert_eq!(wall_clock(&date), "2024-03-15 23:30:00");
    }

    #[test]
    fn date_created_outranks_create_date() {
        let date = read(&packet(
            r#"<rdf:Description rdf:about=""
                 xmp:CreateDate="2020-01-01T09:00:00+00:00"
                 photoshop:DateCreated="2022-02-02T10:00:00+00:00"/>"#,
        ))
        .expect("a date");

        assert_eq!(date.property, Property::PhotoshopDateCreated);
    }

    /// The one departure from plain precedence, and the reason it exists: an
    /// Adobe export writes `photoshop:DateCreated` to day precision beside a
    /// full `xmp:CreateDate`, and taking the higher-ranked one would file the
    /// photograph at midnight while the hour sat in the same file.
    #[test]
    fn a_value_that_knows_the_hour_beats_a_higher_ranked_one_that_does_not() {
        let date = read(&packet(
            r#"<rdf:Description rdf:about=""
                 photoshop:DateCreated="2024-03-15"
                 xmp:CreateDate="2024-03-15T23:30:00+08:00"/>"#,
        ))
        .expect("a date");

        assert_eq!(date.property, Property::XmpCreateDate);
        assert_eq!(wall_clock(&date), "2024-03-15 23:30:00");
        assert!(date.has_time_of_day);
    }

    /// And when nothing knows the hour, rank decides after all.
    #[test]
    fn between_two_date_only_values_rank_still_decides() {
        let date = read(&packet(
            r#"<rdf:Description rdf:about=""
                 xmp:CreateDate="2020-01-01"
                 photoshop:DateCreated="2024-03-15"/>"#,
        ))
        .expect("a date");

        assert_eq!(date.property, Property::PhotoshopDateCreated);
        assert_eq!(wall_clock(&date), "2024-03-15 00:00:00");
        assert!(!date.has_time_of_day);
    }

    // -----------------------------------------------------------------
    // Namespaces
    // -----------------------------------------------------------------

    /// The URI is the property's identity; the prefix is a spelling. A file that
    /// binds the XMP namespace to `q` is saying exactly the same thing.
    #[test]
    fn a_property_is_recognised_under_any_prefix_bound_to_its_namespace() {
        let date = read(
            r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
                        xmlns:q="http://ns.adobe.com/xap/1.0/">
                 <rdf:Description q:CreateDate="2024-03-15T23:30:00+08:00"/>
               </rdf:RDF>"#,
        )
        .expect("a date under a non-standard prefix");

        assert_eq!(date.property, Property::XmpCreateDate);
    }

    /// And the converse: the customary prefix bound to somebody else's namespace
    /// is somebody else's property.
    #[test]
    fn the_customary_prefix_bound_elsewhere_is_not_the_property() {
        assert_eq!(
            read(
                r#"<rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
                            xmlns:xmp="http://example.invalid/not-xmp/">
                     <rdf:Description xmp:CreateDate="2024-03-15T23:30:00+08:00"/>
                   </rdf:RDF>"#
            ),
            None
        );
    }

    /// A sidecar that lost its `xmlns` declarations is still legible to a
    /// person, and `xmp:CreateDate` still means what it says. Falling back to
    /// the prefix costs nothing and recovers a file that would otherwise take a
    /// filesystem date.
    #[test]
    fn an_undeclared_prefix_falls_back_to_its_customary_meaning() {
        let date = read(r#"<rdf:Description xmp:CreateDate="2024-03-15T23:30:00+08:00"/>"#)
            .expect("a date under an unbound but customary prefix");

        assert_eq!(date.property, Property::XmpCreateDate);
    }

    /// An unprefixed name is not an XMP property, and reading one would take a
    /// date out of any XML file that happened to use the word.
    #[test]
    fn a_bare_unprefixed_name_is_not_a_property() {
        assert_eq!(
            read(r#"<Description CreateDate="2024-03-15T23:30:00"/>"#),
            None
        );
        assert_eq!(read("<CreateDate>2024-03-15T23:30:00</CreateDate>"), None);
    }

    // -----------------------------------------------------------------
    // Value forms
    // -----------------------------------------------------------------

    #[test]
    fn the_date_spellings_exporters_write_all_parse() {
        let cases = [
            ("2024-03-15T23:30:00+08:00", "2024-03-15 23:30:00", true),
            ("2024-03-15T23:30:00-05:30", "2024-03-15 23:30:00", true),
            ("2024-03-15T23:30:00Z", "2024-03-15 23:30:00", true),
            (
                "2024-03-15T23:30:00.123456+08:00",
                "2024-03-15 23:30:00",
                true,
            ),
            ("2024-03-15T23:30:00", "2024-03-15 23:30:00", true),
            ("2024-03-15T23:30", "2024-03-15 23:30:00", true),
            ("2024:03:15 23:30:00", "2024-03-15 23:30:00", true),
            ("2024-03-15", "2024-03-15 00:00:00", false),
        ];

        for (text, expected, has_time) in cases {
            let (naive, _, read_time) =
                parse_value(text).unwrap_or_else(|| panic!("{text} should parse"));
            assert_eq!(
                naive.format("%Y-%m-%d %H:%M:%S").to_string(),
                expected,
                "{text}"
            );
            assert_eq!(read_time, has_time, "{text}");
        }
    }

    /// The offset is kept when the file states one and left absent when it does
    /// not — which is the difference between believing the sidecar and resolving
    /// against the run's policy.
    #[test]
    fn an_offset_is_kept_only_when_the_value_states_one() {
        assert_eq!(
            parse_value("2024-03-15T23:30:00+08:00").map(|(_, offset, _)| offset),
            Some(FixedOffset::east_opt(8 * 3600))
        );
        assert_eq!(
            parse_value("2024-03-15T23:30:00").map(|(_, offset, _)| offset),
            Some(None)
        );
        assert_eq!(
            parse_value("2024-03-15").map(|(_, offset, _)| offset),
            Some(None)
        );
    }

    /// Coarser than a day would mean inventing one, and an invented day is
    /// indistinguishable in the output tree from a real one.
    #[test]
    fn precision_coarser_than_a_day_is_refused() {
        for coarse in ["2024", "2024-03", "not a date", "", "   "] {
            assert_eq!(parse_value(coarse), None, "{coarse:?} names no day");
        }
    }

    /// Whitespace around a value is the exporter's formatting, not part of the
    /// date — an element-form property is routinely written on its own line.
    #[test]
    fn surrounding_whitespace_does_not_stop_a_value_being_read() {
        let date = read(&packet(
            "<rdf:Description>
               <xmp:CreateDate>
                 2024-03-15T23:30:00+08:00
               </xmp:CreateDate>
             </rdf:Description>",
        ))
        .expect("a date despite the formatting");

        assert_eq!(wall_clock(&date), "2024-03-15 23:30:00");
    }

    /// A nested element's text is its own, not its ancestor's.
    #[test]
    fn text_inside_a_nested_element_is_not_taken_as_the_property_value() {
        assert_eq!(
            read(&packet(
                "<rdf:Description>
                   <xmp:CreateDate><rdf:li>2024-03-15T23:30:00</rdf:li></xmp:CreateDate>
                 </rdf:Description>",
            )),
            None,
            "the date is inside rdf:li, which is not a property this reads"
        );
    }
    /// A sidecar over the cap is refused without being parsed, and one under it
    /// is read as normal.
    ///
    /// Both halves matter. A cap that refused everything would pass the first
    /// assertion alone, and a valid sidecar being ignored is a worse defect
    /// than the memory exhaustion the cap exists to prevent.
    #[test]
    fn a_sidecar_larger_than_the_cap_is_refused_unparsed() {
        let dir = tempfile::tempdir().unwrap();

        let valid = r#"<?xml version="1.0"?>
            <x:xmpmeta xmlns:x="adobe:ns:meta/">
              <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
                <rdf:Description xmlns:xmp="http://ns.adobe.com/xap/1.0/"
                                 xmp:CreateDate="2024-03-15T14:30:00"/>
              </rdf:RDF>
            </x:xmpmeta>"#;

        let small = dir.path().join("small.xmp");
        std::fs::write(&small, valid).unwrap();
        assert!(
            read_date(&small).is_some(),
            "a legitimate sidecar must still be read"
        );

        // The same valid XML, padded past the cap with a comment. Padding rather
        // than junk so the *only* reason it is refused is its size — if the cap
        // were removed this file would still parse to a date, and the test would
        // fail rather than pass for the wrong reason.
        let oversized = dir.path().join("oversized.xmp");
        let padding = " ".repeat(usize::try_from(MAX_SIDECAR_BYTES).unwrap() + 1);
        std::fs::write(&oversized, format!("<!--{padding}-->{valid}")).unwrap();
        assert!(
            std::fs::metadata(&oversized).unwrap().len() > MAX_SIDECAR_BYTES,
            "the fixture must actually exceed the cap"
        );
        assert_eq!(
            read_date(&oversized),
            None,
            "a sidecar past the cap must be refused"
        );
    }
}
