# Xylo identity and artwork

The product name is **Xylo**. Production bundles, notifications and authenticated update policy use `com.qurkos.xylo`. The local macOS preview uses `com.qurkos.xylo.preview` so it can be identified separately from a production installation.

Runtime identity constants live in `crates/core/src/branding.rs`. Packaging metadata repeats the public identity because it is consumed before the application starts; release-script checks verify those values. Native executable and Rust crate names remain stable.

## Existing installations

New installations use the Xylo application data location. Startup reuses an existing legacy profile in place when it contains a database and the new location does not. An empty new directory does not hide an existing database. If both locations contain databases, the explicit Xylo profile takes precedence. Startup never copies, merges or deletes profiles as part of the rename.

The credential service identifier remains `org.agentworkbench` so saved account references continue to address the same operating system credential entries. Plugin package identities, the versioned plugin ABI, journal records and artifact references also keep their compatibility identifiers. These internal identifiers are not interface labels.

Historical validation reports retain the product name, bundle path and hashes recorded when their binaries were tested. They do not qualify a newly branded build.

## Artwork

`packaging/branding/xylo-mark.png` is the original transparent master: a curved four-arm X with a central diamond-shaped opening. `xylo-lockup.png` pairs it with the Xylo wordmark for presentation. The artwork was generated for this product; no ChatGPT or Claude artwork was copied. Generation prompts, source hashes and the lockup provenance are recorded in `packaging/branding/brand.json`.

The native developer utility `prepare_brand_icons` produces size and format derivatives for application bundles, the sidebar and the tray. `packaging/icons/manifest.json` records their hashes and conversion parameters. The generated master stays unchanged. See [distribution](distribution.md) for regeneration and packaging commands.

The sidebar places the transparent mark on a white tile for contrast in both themes. Application icons use a warm-white background. The macOS tray uses the mark's alpha as a monochrome template so the system can apply the appropriate foreground color.
