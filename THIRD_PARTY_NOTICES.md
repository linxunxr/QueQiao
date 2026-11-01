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

## Chromium（专属浏览器，独立资产分发）

- 上游：<https://www.chromium.org/>（Chromium 开源项目）
- 许可：BSD 3-Clause License 等（见资产包内 chrome-win/ 目录随附的许可文件；源码许可 <https://chromium.googlesource.com/chromium/src/+/main/LICENSE>）
- 用途：鹊桥「专属浏览器」功能的浏览器内核，作为独立 Release 资产（queqiao-browser-*-win64.zip）分发，首次启用时下载到安装目录 browser/ 下；不修改上游代码。
- 说明：开源 Chromium 构建不含 H.264/AAC 等专有编解码器，部分站点视频可能无法播放（文字交互不受影响）。

## 语音 ASR 组件（收语音转写，随主包分发）

### sherpa-onnx（含 onnxruntime 静态链接）

- 上游：<https://github.com/k2-fsa/sherpa-onnx>（k2-fsa 项目）
- 许可：Apache-2.0
- 用途：本地离线语音识别运行时（官方 Rust crate `sherpa-onnx`，构建期静态
  链接其预编译库——含 onnxruntime，产物单 exe 无独立 DLL）。

### paraformer-zh 模型（int8，~217MB，不随包分发）

- 上游：ModelScope `iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-pytorch`
  经 k2-fsa 转换为 onnx 发布（<https://github.com/k2-fsa/sherpa-onnx/releases/tag/asr-models>）
- 许可：Apache-2.0
- 用途：中文语音识别模型；用户首次启用时从 hf-mirror.com 下载到本机
  `asr-model/paraformer-zh/`，不出现在发行包与仓库中。

### SILK v3 解码器（vendor Skype SILK 固定点 C 源）

- 上游：Skype Limited 2006 年公开发布的 SILK SDK（本仓库 vendor 于
  `crates/queqiao-media/vendor/silk/`，源文件头保留其版权与许可声明）
- 许可：BSD 风格（源/二进制再分发须保留版权与免责声明）；**源文件头同时
  声明"不授任何方专利权"**——解码用途 + 单用户自托管分发的实际风险低，
  该灰区已经项目维护者评估接受。
- 用途：QQ 语音（silk v3，含腾讯 0x02 前缀变体）解码为 PCM；编译方式
  （cc + 手写 FFI）参照开源项目 rust-silk（BSD-3-Clause）验证过的方案。

### hound

- 上游：<https://github.com/ruuda/hound>
- 许可：Apache-2.0 OR MIT
- 用途：WAV（RIFF/PCM）读取。
