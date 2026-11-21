# The model files

The module's catalogue (`src/catalogue.rs`) names four ONNX files and their SHA-256. This repository never contains the weights. The **omarchy-onnx-models** repository publishes them, together with their conversion scripts and licence notices, so that other projects can reuse them.

Its release assets follow this URL pattern:

```
https://github.com/rsd/omarchy-onnx-models/releases/download/<tag>/<file>
```

The current release is [`v1.0.0`](https://github.com/rsd/omarchy-onnx-models/releases/tag/v1.0.0). Its assets are the four files below, `LICENSE-Apache-2.0`, `NOTICE`, `MANIFEST.toml` and `SHA256SUMS`.

The model packages `omarchy-onnx-model-<id>` (`packaging/arch/PKGBUILD` of that repository, version 1.0.0) download the files from the release and check each against the release's SHA-256, the same as below. A file that differs fails the build. omarchy-broadcast builds none of them. Its module package `omarchy-broadcast-onnx-git` depends on the default model's package.

| Catalogue file | Made from | SHA-256 |
|---|---|---|
| `selfie_segmentation_landscape.onnx` | MediaPipe TFLite, converted with `tf2onnx` at opset 17 and a standard `ConvTranspose` in place of a custom operator | `fa02222275c455638fa1190493b659d25ad31e86f1b6459f17bd501f02cbe43d` |
| `pp_humansegv2_mobile_192x192.onnx` | PaddleSeg inference model, with its input shape fixed, then exported with `paddle2onnx` at opset 13 | `2bca11ec1f2a662146147d97523d613f5ef4ebf2a89803a95a0cbabfe2bd70a9` |
| `pp_humansegv2_lite_192x192.onnx` | The same recipe; `paddle2onnx` raises it to opset 14 | `ca96b4436f1ce6a76059e8902e20a9617f6829fb11f6c6c9e876563e6352e6b4` |
| `modnet_photographic.onnx` | The upstream ONNX export, unmodified | `07c308cf0fc7e6e8b2065a12ed7fc07e1de8febb7dc7839d7b7f15dd66584df9` |

Each model package installs its licences and notices in `/usr/share/licenses/omarchy-onnx-model-<id>/`, beside the file in `/usr/share/omarchy-onnx-models/`. They are in [LICENSES.md](LICENSES.md) and [LICENSE-APACHE-2.0](LICENSE-APACHE-2.0).
