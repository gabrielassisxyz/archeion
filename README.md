# Archeion

Local-first archival tool for web content and its metadata.

Archeion captures a page, keeps the raw response exactly as it arrived, and records the metadata around it: canonical URL, content hash, HTTP status, MIME type, title, author, publication date, OpenGraph and schema.org data, assets, outbound links and collection tags. The archive is a directory on your disk in a layout built to be revisited, re-indexed and reprocessed years later.

## Status

Early, and honest about it: there is no usable command yet. The crawl engine has been chosen and benchmarked, and the item, capture and asset model is the next thing to be designed. Running `archeion` today prints that and exits. [`ROADMAP.md`](ROADMAP.md) tracks what exists, what is missing, and what is deliberately out of scope.

## Install

Every tagged release publishes prebuilt binaries for Linux and macOS, on x86-64 and arm64, with a SHA-256 checksum per asset.

```sh
curl -sSfL https://raw.githubusercontent.com/gabrielassisxyz/archeion/main/install.sh | sh
```

The installer verifies the published checksum before writing to `~/.local/bin`. Set `ARCHEION_INSTALL_DIR` to install elsewhere, or `ARCHEION_VERSION=vX.Y.Z` to pin a version instead of taking the latest.

From source, with a Rust 1.96 or newer toolchain:

```sh
cargo install --git https://github.com/gabrielassisxyz/archeion
```

## Design

- **The archive outlives the tool.** Raw responses are stored verbatim and addressed by content hash, so a later version can re-derive metadata without re-fetching anything.
- **The crawler is a dependency, not the product.** It sits behind an interface this project owns. Canonicalization, dedupe, per-seed deadlines, retry and rate-limit policy, storage layout, extraction, retention, indexing and export are Archeion's job, and they do not change when the engine underneath does.
- **Local first.** The first useful version runs on one machine with no hosted service, no account and no network dependency beyond the sites being archived.
- **Personal archiving, not redistribution.** Authenticated and terms-sensitive content is handled carefully by default.

## Development

```sh
bin/install-hooks   # once after cloning: secret scan and prose gate on every commit
bin/ci              # every gate, exactly what CI runs
```

[`CONTRIBUTING.md`](CONTRIBUTING.md) has the conventions, [`AGENTS.md`](AGENTS.md) is the working spec for humans and coding agents alike.

## License

MIT. See [`LICENSE`](LICENSE).
