//! Sidecars — the small files that belong to a photograph without being one.
//!
//! An XMP sidecar holds the edits Lightroom or darktable made to a RAW file
//! that must never be written into; an Apple `.aae` holds the adjustments the
//! Photos app made to a JPEG; a `.thm` is the thumbnail some camcorders write
//! beside a clip. All three are the same shape of thing: a file whose entire
//! meaning is "I belong to the media file next to me, under that name".
//!
//! That last clause is why they cannot be left to the ordinary machinery. A
//! sidecar is bound to its parent *by filename*, so an organiser that renames
//! `IMG_1234.CR2` to `2024-03-15-143000.cr2` and leaves `IMG_1234.xmp` where it
//! was has not merely misplaced a file — it has severed the link, and the
//! photograph arrives in its new home with every edit its owner made silently
//! detached. A sidecar moved to the *right* directory under its *old* name is
//! no better. The pairing has to be re-established at the destination or it is
//! gone.
//!
//! Three things follow, and they are the whole design:
//!
//! * A sidecar is **never** treated as a media file. It is not scanned as one,
//!   not deduplicated, not dated, not counted in the scan totals — it has no
//!   independent existence.
//! * A sidecar moves **only** when its parent moves, and to wherever the parent
//!   actually landed. Not where the parent was planned to land:
//!   [`crate::organiser::execute_move`] resolves collisions by trying
//!   `photo-1.jpg`, and a sidecar derived from the planned name would be
//!   orphaned by the very suffix that saved the parent.
//! * A sidecar with **no** parent, or with more than one candidate parent, is
//!   left exactly where it is and reported. See [`OrphanReason`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use tracing::debug;

use crate::naming::{sanitise_for_filename, UNNAMED};
use crate::scanner::ScannedFile;

/// The sidecar extensions a run recognises out of the box (lowercase, no dot).
///
/// Three formats, one category, deliberately. They are written by different
/// software for different reasons, and none of that matters here: what the tool
/// does with each of them is identical, so a separate list per format would be
/// three settings that must always be changed together. The list is
/// configurable because the fourth one exists — `.pp3` for `RawTherapee`,
/// `.dop` for `DxO`, `.on1` — and naming it should not require a new release.
pub const DEFAULT_SIDECAR_EXTENSIONS: &[&str] = &["xmp", "aae", "thm"];

/// Which naming convention binds a sidecar to its parent.
///
/// Both are in the wild and the difference is not cosmetic — it decides what the
/// sidecar must be *called* at the destination, so guessing wrong breaks the
/// pairing just as thoroughly as not moving the file at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Convention {
    /// `IMG_1234.xmp` beside `IMG_1234.CR2` — the sidecar's stem is the
    /// parent's stem. Adobe's convention, and the one most tools write.
    ///
    /// Ambiguous by construction: it says nothing about *which* `IMG_1234` it
    /// belongs to, so a directory holding both `IMG_1234.jpg` and
    /// `IMG_1234.cr2` cannot be resolved. See [`OrphanReason::Ambiguous`].
    Stem,
    /// `IMG_1234.CR2.xmp` beside `IMG_1234.CR2` — the sidecar's stem is the
    /// parent's *whole filename*, extension included. darktable's convention,
    /// and the unambiguous one.
    FullName,
}

/// A sidecar file and the convention that paired it with its parent.
///
/// The convention is carried rather than re-derived because it is the only
/// thing that survives the move: at the destination the parent has a new name,
/// so nothing in the two paths still says which of the two shapes bound them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sidecar {
    /// Where the sidecar is now.
    pub path: PathBuf,
    /// How it was paired.
    pub convention: Convention,
}

impl Sidecar {
    /// Where this sidecar belongs, given where its parent actually landed.
    ///
    /// The convention is preserved rather than normalised. A sidecar written as
    /// `IMG_1234.CR2.xmp` lands as `<new stem>.cr2.xmp`, and one written as
    /// `IMG_1234.xmp` lands as `<new stem>.xmp` — because the tool that wrote it
    /// is the tool that will next go looking for it, and it will look under the
    /// convention it uses. Rewriting one form into the other would leave a file
    /// that pairs correctly by our reading and not by its owner's.
    ///
    /// The extension is lowercased, matching what the organiser does to every
    /// other extension it writes: a run that renames `PHOTO.JPG` to
    /// `2024-03-15-143000.jpg` and its companion to `2024-03-15-143000.AAE`
    /// would be inconsistent in a way somebody has to notice by hand.
    ///
    /// Total by construction, like [`crate::organiser::build_target_path`]: the
    /// base comes from `file_name`/`file_stem`, which are single components by
    /// definition, and the extension goes through [`sanitise_for_filename`], so
    /// the result is one ordinary path component below the parent's own
    /// directory whatever `path` holds.
    ///
    /// The base is deliberately *not* sanitised. It is the parent's own
    /// destination name, which the organiser already guaranteed is a single safe
    /// component — and running it through the sanitiser would turn the `.` in
    /// `2024-03-15-143000.cr2` into `_`, so the darktable convention would land
    /// as `2024-03-15-143000_cr2.xmp` and pair with nothing at all.
    #[must_use]
    pub fn destination_beside(&self, parent_destination: &Path) -> PathBuf {
        let base = match self.convention {
            Convention::Stem => parent_destination.file_stem(),
            Convention::FullName => parent_destination.file_name(),
        }
        .map(|base| base.to_string_lossy().into_owned())
        .filter(|base| !base.is_empty())
        .unwrap_or_else(|| UNNAMED.to_string());

        let extension = self
            .path
            .extension()
            .map(|extension| sanitise_for_filename(&extension.to_string_lossy().to_lowercase()))
            .filter(|extension| !extension.is_empty());

        let name = match extension {
            Some(extension) => format!("{base}.{extension}"),
            None => base,
        };

        parent_destination
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(name)
    }
}

/// Why a sidecar is not travelling anywhere.
///
/// Both mean the same thing on disk — the file is left exactly where it is —
/// and they are reported apart because they ask different things of the person
/// reading. One is housekeeping; the other is a question only they can answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanReason {
    /// Nothing in the sidecar's own directory answers to its name. The media
    /// file it was written for has been deleted, moved, or was never scanned —
    /// a `skip_patterns` entry or a narrowed extension list will do it.
    NoParent,
    /// More than one media file answers to its name, and nothing in the sidecar
    /// says which.
    ///
    /// `IMG_1234.xmp` beside both `IMG_1234.jpg` and `IMG_1234.cr2` is the
    /// ordinary case — a RAW+JPEG shooter whose editor wrote a bare-stem
    /// sidecar. Picking one would be a coin toss dressed up as a decision, and
    /// the losing photograph would arrive with somebody else's edits attached.
    /// So the file stays put and the run says so.
    Ambiguous,
}

/// A sidecar left where it was, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Orphan {
    pub path: PathBuf,
    pub reason: OrphanReason,
}

/// Every discovered sidecar, paired with the media file it belongs to.
///
/// Built once per run from the scan's two lists and consulted while planning, so
/// the pairing rules live in one place rather than being re-derived at each of
/// the three points that need them (the organise plan, the duplicate pass, and
/// the preview listing).
#[derive(Debug, Default)]
pub struct SidecarIndex {
    by_parent: HashMap<PathBuf, Vec<Sidecar>>,
    orphans: Vec<Orphan>,
}

impl SidecarIndex {
    /// The index of a run that is not handling sidecars — `--no-sidecars`, or a
    /// caller that has none.
    ///
    /// Distinct from "we looked and found none" only in that nothing was looked
    /// at; both report zero, which is the truth in each case.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Pair each sidecar with its parent, or record why it has none.
    ///
    /// Matching is case-insensitive — the same file is `IMG_1234.CR2` on one
    /// filesystem and `img_1234.cr2` on another, and a pairing that broke on a
    /// case-insensitive volume would break for exactly the users who copy
    /// libraries between machines.
    ///
    /// [`Convention::FullName`] is tried first because it is the more specific
    /// reading of the same filename: `a.jpg.xmp` names the file `a.jpg`
    /// outright, and only if no such file exists is it worth asking whether
    /// something called `a.jpg.<something>` was meant instead.
    #[must_use]
    pub fn build(media: &[ScannedFile], sidecars: &[PathBuf]) -> Self {
        // Keyed on (directory, lowercased name) rather than on a joined string:
        // a directory is not text, and folding one into a key would make
        // `holiday/a` and `holiday` + `/a` the same key on one platform and not
        // on another.
        let mut by_full_name: HashMap<(&Path, String), Vec<&Path>> = HashMap::new();
        let mut by_stem: HashMap<(&Path, String), Vec<&Path>> = HashMap::new();

        for file in media {
            let Some(directory) = file.path.parent() else {
                continue;
            };
            if let Some(name) = lowercased(file.path.file_name()) {
                by_full_name
                    .entry((directory, name))
                    .or_default()
                    .push(&file.path);
            }
            if let Some(stem) = lowercased(file.path.file_stem()) {
                by_stem
                    .entry((directory, stem))
                    .or_default()
                    .push(&file.path);
            }
        }

        let mut index = Self::default();

        for sidecar in sidecars {
            let (Some(directory), Some(stem)) = (sidecar.parent(), lowercased(sidecar.file_stem()))
            else {
                index.orphan(sidecar, OrphanReason::NoParent);
                continue;
            };

            let key = (directory, stem);
            let matched = by_full_name
                .get(&key)
                .map(|parents| (Convention::FullName, parents))
                .or_else(|| by_stem.get(&key).map(|parents| (Convention::Stem, parents)));

            match matched {
                None => index.orphan(sidecar, OrphanReason::NoParent),
                Some((_, parents)) if parents.len() > 1 => {
                    index.orphan(sidecar, OrphanReason::Ambiguous);
                }
                Some((convention, parents)) => {
                    // `parents` holds exactly one entry, which the arm above
                    // established; `first` rather than `[0]` because a panicking
                    // index in a library that moves photo libraries is not worth
                    // the character it saves.
                    //
                    // The `None` arm of this `if let` is consequently
                    // unreachable and shows as uncovered. It is deliberate: an
                    // empty `parents` cannot be constructed here — a key only
                    // exists once something has been pushed under it — and the
                    // alternative to stepping over it is an `unwrap`.
                    if let Some(parent) = parents.first() {
                        debug!(
                            sidecar = %sidecar.display(),
                            parent = %parent.display(),
                            ?convention,
                            "paired a sidecar with its parent"
                        );
                        index
                            .by_parent
                            .entry((*parent).to_path_buf())
                            .or_default()
                            .push(Sidecar {
                                path: sidecar.clone(),
                                convention,
                            });
                    }
                }
            }
        }

        // Sorted so a run is reproducible: the scan's order is the filesystem's,
        // and two machines walking the same tree can hand these back in
        // different orders. A journal whose entries permute between runs is one
        // nobody can diff.
        for sidecars in index.by_parent.values_mut() {
            sidecars.sort_by(|a, b| a.path.cmp(&b.path));
        }
        index.orphans.sort_by(|a, b| a.path.cmp(&b.path));

        index
    }

    fn orphan(&mut self, path: &Path, reason: OrphanReason) {
        debug!(sidecar = %path.display(), ?reason, "sidecar left in place");
        self.orphans.push(Orphan {
            path: path.to_path_buf(),
            reason,
        });
    }

    /// The sidecars that travel with `parent`, in path order.
    #[must_use]
    pub fn for_parent(&self, parent: &Path) -> &[Sidecar] {
        self.by_parent.get(parent).map_or(&[], Vec::as_slice)
    }

    /// Sidecars left where they were, in path order.
    #[must_use]
    pub fn orphans(&self) -> &[Orphan] {
        &self.orphans
    }

    /// How many sidecars found a parent.
    #[must_use]
    pub fn paired(&self) -> usize {
        self.by_parent.values().map(Vec::len).sum()
    }
}

/// A path component, lowercased, or `None` if it is absent.
///
/// Non-UTF-8 goes through `to_string_lossy` rather than being refused: a name
/// this cannot spell exactly is still a name that can be compared against
/// another spelled the same way, and refusing it would silently exclude
/// somebody's files from pairing on the one filesystem where that matters.
fn lowercased(component: Option<&std::ffi::OsStr>) -> Option<String> {
    component.map(|name| name.to_string_lossy().to_lowercase())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a panicking assertion in a test is a failing test, which is the desired signal"
)]
mod tests {
    use super::*;

    fn media(path: &str) -> ScannedFile {
        let path = PathBuf::from(path);
        let extension = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        ScannedFile {
            path,
            size: 1,
            extension,
            is_video: false,
        }
    }

    fn index(media_paths: &[&str], sidecars: &[&str]) -> SidecarIndex {
        let files: Vec<ScannedFile> = media_paths.iter().map(|p| media(p)).collect();
        let sidecars: Vec<PathBuf> = sidecars.iter().map(PathBuf::from).collect();
        SidecarIndex::build(&files, &sidecars)
    }

    fn paired_with(index: &SidecarIndex, parent: &str) -> Vec<(String, Convention)> {
        index
            .for_parent(Path::new(parent))
            .iter()
            .map(|s| (s.path.display().to_string(), s.convention))
            .collect()
    }

    // -----------------------------------------------------------------
    // Pairing
    // -----------------------------------------------------------------

    #[test]
    fn a_stem_match_pairs_a_sidecar_with_its_parent() {
        let index = index(&["/p/IMG_1234.cr2"], &["/p/IMG_1234.xmp"]);

        assert_eq!(
            paired_with(&index, "/p/IMG_1234.cr2"),
            [("/p/IMG_1234.xmp".to_string(), Convention::Stem)]
        );
        assert!(index.orphans().is_empty());
    }

    #[test]
    fn a_full_filename_match_pairs_a_sidecar_with_its_parent() {
        let index = index(&["/p/IMG_1234.cr2"], &["/p/IMG_1234.cr2.xmp"]);

        assert_eq!(
            paired_with(&index, "/p/IMG_1234.cr2"),
            [("/p/IMG_1234.cr2.xmp".to_string(), Convention::FullName)]
        );
    }

    /// The same file is `IMG_1234.CR2` on one volume and `img_1234.cr2` on
    /// another. A pairing that broke on the difference would break for exactly
    /// the people who move libraries between machines.
    #[test]
    fn pairing_ignores_case_on_both_sides() {
        let index = index(&["/p/IMG_1234.CR2"], &["/p/img_1234.XMP"]);

        assert_eq!(
            paired_with(&index, "/p/IMG_1234.CR2"),
            [("/p/img_1234.XMP".to_string(), Convention::Stem)]
        );
    }

    /// `a.jpg.xmp` names `a.jpg` outright. Reading it as a bare stem instead
    /// would only be worth doing if no file called `a.jpg` existed.
    #[test]
    fn the_full_filename_convention_is_preferred_over_the_stem_one() {
        // Both readings have a candidate: `a.jpg` by full name, and `a.jpg.png`
        // by stem. The specific one must win.
        let index = index(&["/p/a.jpg", "/p/a.jpg.png"], &["/p/a.jpg.xmp"]);

        assert_eq!(
            paired_with(&index, "/p/a.jpg"),
            [("/p/a.jpg.xmp".to_string(), Convention::FullName)]
        );
        assert!(paired_with(&index, "/p/a.jpg.png").is_empty());
    }

    /// A sidecar belongs to the file *beside* it. Two directories holding the
    /// same names are two unrelated pairs.
    #[test]
    fn pairing_does_not_cross_directories() {
        let index = index(&["/p/a/IMG.jpg"], &["/p/b/IMG.xmp"]);

        assert!(paired_with(&index, "/p/a/IMG.jpg").is_empty());
        assert_eq!(
            index.orphans(),
            [Orphan {
                path: PathBuf::from("/p/b/IMG.xmp"),
                reason: OrphanReason::NoParent,
            }]
        );
    }

    #[test]
    fn a_sidecar_with_no_parent_is_an_orphan() {
        let index = index(&["/p/other.jpg"], &["/p/IMG_1234.xmp"]);

        assert_eq!(index.paired(), 0);
        assert_eq!(index.orphans()[0].reason, OrphanReason::NoParent);
    }

    /// The RAW+JPEG case. Picking one would be a coin toss, and the losing
    /// photograph would arrive carrying somebody else's edits.
    #[test]
    fn a_sidecar_with_two_candidate_parents_is_ambiguous_and_stays_put() {
        let index = index(
            &["/p/IMG_1234.jpg", "/p/IMG_1234.cr2"],
            &["/p/IMG_1234.xmp"],
        );

        assert_eq!(index.paired(), 0);
        assert_eq!(index.orphans()[0].reason, OrphanReason::Ambiguous);
        assert!(paired_with(&index, "/p/IMG_1234.jpg").is_empty());
        assert!(paired_with(&index, "/p/IMG_1234.cr2").is_empty());
    }

    /// One photograph can have several: darktable writes a `.xmp`, the camera
    /// wrote a `.thm`, and both belong to the same clip.
    #[test]
    fn one_parent_can_carry_several_sidecars_in_path_order() {
        let index = index(
            &["/p/CLIP.mp4"],
            &["/p/CLIP.xmp", "/p/CLIP.thm", "/p/CLIP.mp4.xmp"],
        );

        let paired = paired_with(&index, "/p/CLIP.mp4");
        assert_eq!(paired.len(), 3);
        assert_eq!(
            paired.iter().map(|(p, _)| p.as_str()).collect::<Vec<_>>(),
            ["/p/CLIP.mp4.xmp", "/p/CLIP.thm", "/p/CLIP.xmp"],
            "sorted, so two machines walking the same tree journal the same order"
        );
    }

    /// A path with no parent directory is `/` itself, and nothing about it is
    /// a photograph. It has to be stepped over rather than indexed under a key
    /// built from a directory that does not exist — the alternative is a panic
    /// or a pairing against the root of the volume.
    #[test]
    fn a_media_path_with_no_parent_directory_is_stepped_over() {
        let index = index(&["/", "/p/IMG_1234.cr2"], &["/p/IMG_1234.xmp"]);

        assert_eq!(index.paired(), 1);
        assert_eq!(
            paired_with(&index, "/p/IMG_1234.cr2"),
            [("/p/IMG_1234.xmp".to_string(), Convention::Stem)]
        );
    }

    /// And the same on the sidecar side: something with no parent and no stem
    /// can be paired with nothing, so it is reported as the orphan it is
    /// rather than silently dropped from the run's tally.
    #[test]
    fn a_sidecar_path_with_no_parent_directory_is_an_orphan() {
        let index = index(&["/p/IMG_1234.cr2"], &["/"]);

        assert_eq!(index.paired(), 0);
        assert_eq!(index.orphans().len(), 1);
        assert_eq!(index.orphans()[0].reason, OrphanReason::NoParent);
    }

    #[test]
    fn an_empty_index_pairs_nothing_and_reports_nothing() {
        let index = SidecarIndex::empty();
        assert_eq!(index.paired(), 0);
        assert!(index.orphans().is_empty());
        assert!(index.for_parent(Path::new("/p/a.jpg")).is_empty());
    }

    // -----------------------------------------------------------------
    // Destination derivation
    // -----------------------------------------------------------------

    #[test]
    fn a_stem_sidecar_takes_the_parents_new_stem() {
        let sidecar = Sidecar {
            path: PathBuf::from("/in/IMG_1234.xmp"),
            convention: Convention::Stem,
        };

        assert_eq!(
            sidecar.destination_beside(Path::new("/out/2024-03-15/2024-03-15-143000.cr2")),
            Path::new("/out/2024-03-15/2024-03-15-143000.xmp")
        );
    }

    /// The convention is preserved, not normalised: the tool that wrote this
    /// file is the one that will next go looking for it.
    #[test]
    fn a_full_name_sidecar_takes_the_parents_whole_new_filename() {
        let sidecar = Sidecar {
            path: PathBuf::from("/in/IMG_1234.CR2.xmp"),
            convention: Convention::FullName,
        };

        assert_eq!(
            sidecar.destination_beside(Path::new("/out/2024-03-15/2024-03-15-143000.cr2")),
            Path::new("/out/2024-03-15/2024-03-15-143000.cr2.xmp")
        );
    }

    /// The parent that landed under a collision suffix is the parent the
    /// sidecar has to follow. Deriving from the *planned* name would orphan the
    /// sidecar with the very suffix that saved the photograph.
    #[test]
    fn the_destination_follows_where_the_parent_actually_landed() {
        let sidecar = Sidecar {
            path: PathBuf::from("/in/IMG_1234.xmp"),
            convention: Convention::Stem,
        };

        assert_eq!(
            sidecar.destination_beside(Path::new("/out/d/2024-03-15-143000-7.jpg")),
            Path::new("/out/d/2024-03-15-143000-7.xmp")
        );
    }

    #[test]
    fn the_extension_is_lowercased_like_every_other_one_the_tool_writes() {
        let sidecar = Sidecar {
            path: PathBuf::from("/in/IMG_1234.AAE"),
            convention: Convention::Stem,
        };

        assert_eq!(
            sidecar.destination_beside(Path::new("/out/d/photo.jpg")),
            Path::new("/out/d/photo.aae")
        );
    }

    /// Total, like everything else that builds a destination. `Path::extension`
    /// cannot itself contain a separator — it is carved out of `file_name` — so
    /// the sanitiser here is the invariant belonging to the function rather than
    /// to the discipline of its callers, and this pins that it holds for text
    /// the filesystem *can* produce.
    #[test]
    fn an_awkward_extension_still_yields_one_ordinary_component() {
        let sidecar = Sidecar {
            path: PathBuf::from("/in/x.x m p"),
            convention: Convention::Stem,
        };
        let derived = sidecar.destination_beside(Path::new("/out/d/photo.jpg"));

        assert_eq!(derived.parent(), Some(Path::new("/out/d")));
        assert_eq!(
            derived.file_name().map(|n| n.to_string_lossy()).as_deref(),
            Some("photo.x-m-p")
        );
    }

    /// The other half of the same invariant: the parent's own name is left
    /// alone. Sanitising it would put an `_` where the darktable convention
    /// needs a `.`, and the sidecar would pair with nothing.
    #[test]
    fn the_parents_name_is_never_rewritten_on_the_way_through() {
        let sidecar = Sidecar {
            path: PathBuf::from("/in/IMG.cr2.xmp"),
            convention: Convention::FullName,
        };

        assert_eq!(
            sidecar.destination_beside(Path::new("/out/d/2024-03-15-143000.cr2")),
            Path::new("/out/d/2024-03-15-143000.cr2.xmp"),
        );
    }

    #[test]
    fn a_sidecar_with_no_extension_keeps_the_parents_stem_alone() {
        let sidecar = Sidecar {
            path: PathBuf::from("/in/IMG_1234"),
            convention: Convention::Stem,
        };

        assert_eq!(
            sidecar.destination_beside(Path::new("/out/d/photo.jpg")),
            Path::new("/out/d/photo")
        );
    }
}
