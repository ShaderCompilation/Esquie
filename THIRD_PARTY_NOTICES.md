# Third-party notices

Esquie's own code, documentation, and artwork are Copyright © 2026 Hayk Sahakyan
and licensed under GPL-3.0-only. Third-party components retain their own licenses
and copyright notices. This overview does not replace their license texts.

## Rust dependencies and Slint

Cargo.lock records the dependency versions and Git revision used by the build.
The AppImage carries generated `dependencies.html`, `cargo-notices.html`, and
`dependencies.json` beside this file. They identify the locked Linux dependency
graph, chosen license texts, and additional upstream copyright/license files.
Build dependencies are included; dependencies used only by tests are excluded.
Slint and its internal crates are used under **GPL-3.0-only**; their alternative
royalty-free and commercial terms are not the licensing basis for this build.
Slint is Copyright © SixtyFPS GmbH and its contributors.

The generated inventory is supplemented by reviewed upstream license files where
published crates omit them. Their exact source revisions and SHA-256 digests are
tracked in `packaging/licenses/sources.json`. Dependency upgrades require reviewing
these inputs; missing upstream notices cause generation to fail.

## DeepFilterNet and the embedded model

Esquie uses [DeepFilterNet](https://github.com/Rikorose/DeepFilterNet) v0.5.6,
locked to revision `978576aa8400552a4ce9730838c635aa30db5e61`. It embeds
`models/DeepFilterNet3_onnx.tar.gz` through libDF's `default-model` feature.
DeepFilterNet code and model weights are available under MIT or Apache-2.0;
this distribution uses the MIT option. The accompanying `deepfilternet.txt`
retains Copyright (c) 2021 Hendrik Schröter and the MIT terms.

The maintainer explicitly [clarified that the license covers model weights](https://github.com/Rikorose/DeepFilterNet/commit/7b14b89).
The pinned v0.5.6 README predates that clarification. The model remains an
upstream asset; Esquie does not claim authorship or relicense it.

## AppImage runtime

The AppImage contains the unmodified x86_64
[type 2 runtime, release 20251108](https://github.com/AppImage/type2-runtime/tree/20251108),
pinned by checksum in `packaging/appimage/tools.sha256`.
`appimage-runtime.txt` and `appimage-runtime-source-header.txt` retain its MIT
terms and notices, including Simon Peter, Alexander Larsson, and the public-domain
MD5 implementation attribution.

Its upstream build recipe incorporates these libraries:

| Component | Terms and accompanying file |
| --- | --- |
| musl libc | MIT and incorporated notices in `musl.txt` |
| libfuse 3.15.0 | LGPL-2.1-or-later library; `libfuse-LGPL-2.1.txt` and `libfuse-GPL-2.0.txt` |
| squashfuse 0.5.2 | BSD-2-Clause; `squashfuse.txt` |
| Zstandard | BSD-3-Clause option; `zstd.txt` |
| zlib | Zlib; `zlib.txt` |
| mimalloc | MIT; `mimalloc.txt` |

The runtime's Makefile links mimalloc in addition to the libraries listed in its
LICENSE. License-text snapshots and their upstream URLs are in
`packaging/licenses/sources.json`; snapshot versions do not assert the exact
Alpine package revisions incorporated in upstream's prebuilt runtime. Obtain
runtime build/source provenance when preparing corresponding source, including
the upstream libfuse patch, static-linking/relinking materials required by LGPL,
and the actual incorporated dependency sources.

linuxdeploy and appimagetool are build tools, not applications copied into the
AppDir. Any files they add to the payload must be reviewed for additional notices.
Esquie deliberately uses the host's PipeWire, glibc, display libraries, and graphics
drivers. Those host components are not included merely because Esquie links them.

## Finding the texts and source

After extracting the AppImage, these files are under
`squashfs-root/usr/share/doc/esquie/`. From a source checkout, follow
[the build instructions](docs/building.md#license-documentation) to regenerate them.

Binary distributors must provide applicable license notices and access to matching
corresponding source under GPLv3 and applicable dependency terms. For each binary,
provide clear source-download directions alongside it and retain the exact Esquie
source, lockfile, dependency/model sources, and build/install scripts. A source
link or license report alone does not satisfy all static LGPL relinking obligations.
