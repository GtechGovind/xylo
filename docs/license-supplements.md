# Dependency license supplements

The catalogue now covers 121 exact registry-package versions: 68 with pinned upstream full texts and 53 with a separately identified canonical-standard-text policy. The canonical mappings select Apache-2.0 for 36 explicit grants, MIT for 16 grants and CC0-1.0 for one grant. All 121 mappings have been checked against the current Cargo.lock and original cached crate archives. This establishes technical text/provenance coverage for these mappings, not legal approval, a newly generated target closure, or complete binary-release acceptance.

The schema-2 catalogue in `packaging/licenses/supplements.json` contains 390 text/source associations backed by 376 byte-preserved files. Its new records include three complete canonical standards, 333 exact publication files, two upstream full licenses, and five pinned objc2 guidance associations. Existing copyright, exception, source-header and author metadata is preserved. Guidance, readmes, SPDX identifiers and author lists remain auxiliary notices; none is relabeled as complete license text.

## Two explicit provenance policies

`upstream_file` is the existing policy. A license/notice file must come from the package's exact source repository and publication commit, or from an explicitly verified version-tag/source binding. A license file from a similarly named project, another version, or the default branch does not qualify.

`canonical_standard` is a distinct policy for an exact publication that grants an unambiguous supported license but omits its full standard text. The standards are the complete [MIT](https://github.com/spdx/license-list-data/blob/31ba1a50e5397e00a304dbadc76531740e89ee48/text/MIT.txt) and [Apache-2.0](https://github.com/spdx/license-list-data/blob/31ba1a50e5397e00a304dbadc76531740e89ee48/text/Apache-2.0.txt) and [CC0-1.0](https://github.com/spdx/license-list-data/blob/31ba1a50e5397e00a304dbadc76531740e89ee48/text/CC0-1.0.txt) texts from SPDX License List 3.29.0, pinned to commit `31ba1a50e5397e00a304dbadc76531740e89ee48`. Their source pins and content hashes are independently allowlisted in the resolver; changing catalogue hashes cannot substitute another document. They are explicitly identified as standards from SPDX, never as files obtained from the crate's repository.

| Standard | SHA256 | Git blob SHA1 |
| --- | --- | --- |
| MIT | `b05785f9f18e6716bab63424b11454513b9943a222595b70411009202fc592b5` | `d817195dad53ec992418c28ffca5fbd1cd86502a` |
| Apache-2.0 | `074e6e32c86a4c0ef8b3ed25b721ca23aca83df277cd88106ef7177c354615ff` | `137069b823873b8bcf42979bcf8e9371052d26a2` |
| CC0-1.0 | `a2010f343487d3f7618affe54f789f5487602331c0a8d03f49e9a7c547cf0499` | `0e259d42c996742e9e3cba14c677129b2c1b6311` |

The canonical policy requires the exact registry source, package/version, Cargo.lock archive checksum, normalized Cargo.toml bytes, published repository, license expression, author-field presence and author values. The complete normalized/original manifests are copied as `registry_archive` notices. The selected license must be an explicit alternative in that exact grant; Apache-2.0 is selected whenever permitted. Unsupported/AND/custom/ambiguous expressions and conflicting original/normalized grants are refused. New versions receive no automatic mapping.

`registry_archive` notices retain exact bytes from that checksum-bound publication: manifests, publication VCS metadata when present, root readmes/notices and source files containing actual copyright headers. A bounded scan requires these files to remain in the mapping. Publication members/notices are bounded to 4 MiB, and upstream/standard texts to 128 KiB. Khronos XML files are preserved whole with their embedded copyright/license comments, without excerpts or truncation; this is packaged notice data and is not loaded by the application. Anything larger requires an explicit coverage review. SHA256 and Git-blob content hashes verify these bytes without claiming a GitHub origin for them.

The MIT standard's `<year>` and `<copyright holders>` are unchanged standard-template placeholders. They are not an attribution to Xylo or an assertion about a dependency's owner. Published Cargo authors are retained verbatim as author metadata, without inferring copyright ownership or inventing names or dates. Where a publication omits authors, that absence is explicitly recorded; actual source notices and upstream guidance remain preserved. A conflicting or ambiguous recorded grant/attribution fails validation.

## Verification and copied release results

`scripts/release_licenses.py` resolves supplements offline. The original `.crate` must be a regular cached file whose SHA256 matches Cargo.lock. Upstream ownership checks remain in force for upstream files. Canonical texts instead require their independently pinned authority and exact publication grant. Path traversal, symbolic links, edited bytes, changed repository/license/author metadata, cross-package notices and omitted collected notices are rejected.

`scripts/release_metadata.py` copies the full texts and auxiliary notices into each package's notice folder. `licenses.json` preserves each source class, source URL or archive member, content hash and selected license. `license-supplements.json` preserves the applied publication proofs and catalogue hash. Copied-package validation rechecks canonical authority, grant bindings, original/normalized metadata consistency and every required notice. It does not collapse the SPDX authority into a package's upstream repository.

Production packaging invokes strict inventory validation before releasing binaries. A newly encountered unresolved package still blocks it. The packager carries the generated dependency directory through the macOS, Windows and Linux resource configuration; each actual target needs a fresh inventory and packaged-resource check. These supplement changes do not certify an old or different target inventory.

```sh
python3 scripts/release_metadata.py --validate-inventory /path/to/generated-notices
python3 scripts/release_metadata.py --validate-inventory /path/to/generated-notices --strict
PYTHONDONTWRITEBYTECODE=1 python3 -m unittest discover -s scripts -p 'test_release*.py' -v
```

The focused license suite has 24 tests. It exercises actual generated notice/provenance copies, an `.app` resource-copy fixture, archive/lock/source/version mismatches, edited texts, wrong ownership, symlinks/traversal, dirty-publication transforms, tag bindings, guidance-only strict refusal, altered canonical authorities/bytes/grants/authors, unsupported alternatives, conflicting original grants, omitted source notices, and copied-inventory tampering. All 121 real cached archives were additionally resolved without compiling Cargo code. Separate release-pipeline tests remain in their own targets.

## Exceptional publications and separate obligations

`audio-core 0.2.1` still records `dirty=true`. All 24 source files match its publication commit, and Cargo.toml.keep matches the pinned upstream manifest. The published Cargo.toml.orig differs only by removing an unused dev-dependency table. Every source binding and that exact parsed transform is checked; arbitrary dirty changes are refused.

`sqlite-vec 0.1.7` lacks publication VCS metadata. Its exact tag resolves to a fixed commit; the Rust sources and main C source match it, and its VERSION/manifest template reproduce the published original manifest. This crate-level proof does not independently qualify its external/generated SQLite headers.

The initial 35 canonical mappings do not pretend that missing upstream files were found. For example, pathfinder_geometry's recorded publication commit was unavailable during research; its canonical policy uses its exact registry archive/grant and retains all nine copyright-bearing source files. The newer objc2 family retains its complete pinned [licensing guidance](https://github.com/madsmtm/objc2/blob/7b1abfd750a2cacaea71d6a56ecfb83cb7de560b/LICENSE.md), including its explicit uncertainty about Apple SDK-derived bindings. That guidance remains a separate caveat, not a full license or an Apple distribution permission.

Native libraries and source bundles, generated SDK bindings, fonts/icons, model and speech weights, executable download terms, signing/notarization, and platform redistribution requirements require their own notices and acceptance. Existing package-native text discovery is unchanged; a discovered NOTICE is not newly audited as a complete license by this work. Strict Rust supplement coverage is therefore one technical release gate, never legal approval or proof of every redistribution obligation.

The additional target-only mappings include exact pinned upstream Unlicense for ksni and BSD-2-Clause for libwayshot-xcap, with their original notices. Other explicit single/OR publication grants use the same canonical policy. Initial metadata-only inventory checks passed both macOS closures and correctly blocked three Windows and 17 Linux target-only gaps. Fresh six-target strict checks must be regenerated for the expanded catalogue before a binary-release claim.

The older 66-package/35-gap research and validation files under `docs/evidence/` remain byte-exact historical evidence of the prior upstream-only policy. They are not rewritten to imply that those earlier runs used the canonical policy. Fresh target inventories and packaged checks must establish the current release state.

## Updating a mapping

1. Verify the exact locked archive and its Cargo metadata. Preserve its actual notices before choosing a policy.
2. Prefer exact upstream files with immutable source bindings. If those are absent, explicitly review the publication grant for the canonical policy; do not relabel guidance or silently infer an AND license's obligations.
3. Preserve full standard/upstream bytes and exact publication notices separately. Never fill template copyright placeholders with inferred authors.
4. Extend the curated rules and negative tests for any new authority or expression. Resolve the real archive offline, then regenerate and strictly validate every target inventory and packaged notice directory. Leave genuinely unresolved grants visible and blocked.
