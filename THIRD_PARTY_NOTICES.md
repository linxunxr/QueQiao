# 第三方依赖许可声明（THIRD_PARTY_NOTICES）

本文件记录 queqiao 发布物所含第三方 Rust 依赖的许可信息，履行各宽松许可的
声明保留义务。审计方法：`cargo license --avoid-build-deps --avoid-dev-deps`
（基于 Cargo.lock 全量闭包，2026-10-22，181 个 crate）。

## 结论

**全部为宽松（permissive）许可，无 GPL/LGPL/AGPL/SSPL 类传染性许可，与本仓库
的 MIT 发布完全兼容。**分发二进制时随附本文件即满足各许可的声明保留要求；
各许可完整文本见对应 crate 源码或 [SPDX 许可清单](https://spdx.org/licenses/)。

## 按许可分组

### MIT（32）

axum, axum-core, bytes, data-encoding, generic-array, http-body, http-body-util,
hyper, hyper-util, mio, redox_syscall, slab, synstructure, tokio, tokio-macros,
tokio-tungstenite, tower, tower-http, tower-layer, tower-service, tracing,
tracing-core, try-lock, unsafe-libyaml, want, zmij
（另有本仓库自身 6 个 crate：queqiao 系列，见 [LICENSE](LICENSE)）

### Apache-2.0 OR MIT（107）

atomic-waker, base64, bitflags, block-buffer, bumpalo, cfg-if, chacha20,
const-oid, cpufeatures, crypto-common, digest, displaydoc, equivalent, errno,
form_urlencoded, futures-channel, futures-core, futures-io, futures-macro,
futures-sink, futures-task, futures-util, getrandom, hashbrown, hmac, http,
httparse, httpdate, hybrid-array, idna, idna_adapter, indexmap, ipnet, itoa,
js-sys, libc, lock_api, log, mime, once_cell, parking_lot, parking_lot_core,
percent-encoding, pin-project-lite, proc-macro2, quinn, quinn-proto, quinn-udp,
quote, rand, rand_core, rand_pcg, regex, regex-automata, regex-syntax, reqwest,
rustc-hash, rustls-pki-types, scopeguard, serde, serde_core, serde_derive,
serde_json, serde_path_to_error, serde_urlencoded, serde_yaml, sha1, sha2,
signal-hook-registry, smallvec, socket2, stable_deref_trait, syn, thiserror,
thiserror-impl, tokio-rustls, tungstenite, typenum, url, utf8_iter, uuid,
wasm-bindgen, wasm-bindgen-futures, wasm-bindgen-macro, wasm-bindgen-macro-support,
wasm-bindgen-shared, web-sys, web-time, windows-link, windows-sys,
windows-targets, windows_aarch64_gnullvm, windows_aarch64_msvc,
windows_i686_gnu, windows_i686_gnullvm, windows_i686_msvc, windows_x86_64_gnu,
windows_x86_64_gnullvm, windows_x86_64_msvc, zeroize

### Apache-2.0（1）

sync_wrapper

### Apache-2.0 AND ISC（1，双许可同时适用）

ring

### Apache-2.0 AND Unicode-3.0（1，组合）

unicode-ident

### Apache-2.0 OR Apache-2.0 WITH LLVM-exception OR MIT（1）

wasi

### Apache-2.0 OR BSL-1.0（1）

ryu

### Apache-2.0 OR ISC OR MIT（2）

hyper-rustls, rustls

### Apache-2.0 OR LGPL-2.1-or-later OR MIT（1；分发按 MIT 侧履行）

r-efi

### Apache-2.0 OR MIT OR Zlib（2）

lru-slab, tinyvec

### BSD-3-Clause（1）

subtle

### BSD-3-Clause AND MIT（1）

matchit

### ISC（2）

rustls-webpki, untrusted

### MIT OR Unlicense（2）

aho-corasick, memchr

### Unicode-3.0（18）

icu_collections, icu_locale_core, icu_normalizer, icu_normalizer_data,
icu_properties, icu_properties_data, icu_provider, litemap, potential_utf,
tinystr, writeable, yoke, yoke-derive, zerofrom, zerofrom-derive, zerotrie,
zerovec, zerovec-derive

### CDLA-Permissive-2.0（2；根证书数据集的数据许可）

webpki-roots

## 义务说明

- MIT / BSD / ISC / Zlib / BSL / Unlicense / Unicode-3.0 / CDLA-Permissive-2.0：
  分发时保留版权与许可声明（本文件 + 各 crate 内文本即满足）；
- Apache-2.0：同上；若上游 crate 附带 NOTICE 文件则一并保留（经查本依赖集内
  无附带 NOTICE 的 crate）；
- 双许可（OR）条目按其中任一许可履行即可，以上分组已按惯例归并展示。
