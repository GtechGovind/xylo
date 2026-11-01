//! Built-in review prompts shared by native and headless command surfaces.
/// The focus requested for a new review turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewKind {
    General,
    Security,
}
/// Returns the built-in prompt without starting a turn or granting tool access.
pub fn review_prompt(kind: ReviewKind) -> &'static str {
    match kind {
        ReviewKind::General => include_str!("../prompts/review.md"),
        ReviewKind::Security => include_str!("../prompts/security_review.md"),
    }
    .trim_end()
}
