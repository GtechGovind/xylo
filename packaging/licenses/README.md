# Pinned registry license supplements

Schema 2 distinguishes `upstream_file`, `canonical_standard`, and
`registry_archive` sources. Canonical MIT/Apache/CC0 texts are byte-preserved SPDX
License List 3.29.0 standards, not files claimed to originate in a crate's
repository. They apply only with an exact checksum-bound publication grant and
its preserved manifest/author/source notices. Template copyright placeholders
are not filled in; Cargo authors are not asserted to be copyright owners.

The catalogue and texts are consumed by `scripts/release_metadata.py` through
`scripts/release_licenses.py`. Do not edit an upstream text to fit a template or
point a source URL at a moving branch. Keep exact bytes, source/version ownership
and hashes together. Guidance and auxiliary notices do not satisfy full-text
coverage. This catalogue is not legal or release approval.

See `docs/license-supplements.md` for the verification model, remaining gaps,
exceptional publication proofs and update procedure.
