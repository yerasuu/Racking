//! Fails the build when the workspace version has no release notes, so a version
//! bump cannot ship without its `tag-changelog/vX.Y.Z.md`. The file is keyed off
//! `MAJOR.MINOR.PATCH` only, so the test builds `X.Y.Z-N` share the notes of the
//! stable `X.Y.Z` they lead to (decision #41).

/// Notes of the version being built; embedded only to make the build depend on the file.
pub const NOTES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../tag-changelog/v",
    env!("CARGO_PKG_VERSION_MAJOR"),
    ".",
    env!("CARGO_PKG_VERSION_MINOR"),
    ".",
    env!("CARGO_PKG_VERSION_PATCH"),
    ".md"
));
