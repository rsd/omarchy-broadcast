# Licences of the cut-out models

The open-model cut-out runs four third-party models. Their weights are not in this repository: they are installed beside the module (see the module's [README](../README.md#models)). Each is distributed under the Apache License, Version 2.0, whose text is in [LICENSE-APACHE-2.0](LICENSE-APACHE-2.0) and must be shipped with the files.

**Modified files (Apache-2.0 §4(b)).** The MediaPipe and PP-HumanSeg files are not the upstream files: they were converted by the omarchy-broadcast project from the upstream releases to ONNX (MediaPipe: TFLite to ONNX, with one custom operator replaced by standard ONNX operators; PP-HumanSeg: a Paddle inference model re-saved with a static input shape, then exported to ONNX). The conversion changes the file format, not the weights. How each file was made, and its checksum, is recorded in the omarchy-onnx-models repository, which publishes the files (see [README.md](README.md)). The MODNet file is the upstream ONNX export, unmodified.

**Notices.** The evaluation recorded no upstream `NOTICE` file for any of the three projects; the notice blocks below are the attribution this project ships with each file (Apache-2.0 §4(c), §4(d)). The copyright lines follow the upstream licence files and sources as far as the evaluation read them; check them against the upstream release when packaging.

## MediaPipe Selfie Segmentation (landscape, 144x256)

| | |
|---|---|
| Catalogue id | `mediapipe-landscape` |
| File | `selfie_segmentation_landscape.onnx` (converted by us from `selfie_segmentation_landscape.tflite`) |
| Upstream | Google, MediaPipe: <https://github.com/google-ai-edge/mediapipe> |
| Licence | Apache-2.0 (code: <https://github.com/google-ai-edge/mediapipe/blob/master/LICENSE>; weights: the model card states "LICENSED UNDER Apache License, Version 2.0") |
| Model card | <https://storage.googleapis.com/mediapipe-assets/Model%20Card%20MediaPipe%20Selfie%20Segmentation.pdf> |
| Training data | Described in the model card (Google's own data set); no further terms stated |

Attribution and notice:

```
MediaPipe Selfie Segmentation (landscape)
Copyright The MediaPipe Authors.
Licensed under the Apache License, Version 2.0.
This file was converted from TensorFlow Lite to ONNX by the omarchy-broadcast
project (tf2onnx, then a standard ConvTranspose in place of the TFLite custom
operator Convolution2DTransposeBias); the weights are unchanged.
```

## PP-HumanSeg v2 (mobile and lite, 192x192)

| | |
|---|---|
| Catalogue ids | `pphumanseg-v2-mobile`, `pphumanseg-v2-lite` |
| Files | `pp_humansegv2_mobile_192x192.onnx`, `pp_humansegv2_lite_192x192.onnx` (converted by us from `human_pp_humansegv2_{mobile,lite}_192x192_inference_model_with_softmax`) |
| Upstream | Baidu, PaddleSeg, PP-HumanSeg: <https://github.com/PaddlePaddle/PaddleSeg/tree/release/2.9/contrib/PP-HumanSeg> |
| Licence | Apache-2.0: <https://github.com/PaddlePaddle/PaddleSeg/blob/release/2.9/LICENSE>. The weights are released inside PaddleSeg, with no separate weights licence |
| Training data | **UNCONFIRMED.** The models are trained on PP-HumanSeg14K among other data. PP-HumanSeg14K is available only on request from Baidu (`paddleseg@baidu.com`), with no licence stated publicly. Whether its terms reach the weights has not been confirmed |

Attribution and notice:

```
PP-HumanSeg v2 (mobile, lite), from PaddleSeg
Copyright (c) 2021 PaddlePaddle Authors. All Rights Reserved.
Licensed under the Apache License, Version 2.0.
These files were converted from Paddle inference models to ONNX by the
omarchy-broadcast project (the input shape fixed at 1x3x192x192, then
paddle2onnx at opset 13, which raises it to 14 for lite); the weights are unchanged.
```

## MODNet (photographic)

| | |
|---|---|
| Catalogue id | `modnet` |
| File | `modnet_photographic.onnx` (the upstream ONNX export, unmodified; byte-identical to Hugging Face `Xenova/modnet`) |
| Upstream | Zhanghan Ke et al., MODNet: <https://github.com/ZHKKKe/MODNet> |
| Licence | Apache-2.0: the README states "The code, models, and demos in this repository … are released under the Apache License 2.0" |
| Training data | **UNCONFIRMED**: not named in the README (the paper describes it) |

Attribution and notice:

```
MODNet: Trimap-Free Portrait Matting in Real Time
Copyright The MODNet authors (Zhanghan Ke et al.).
Licensed under the Apache License, Version 2.0.
```

## Not used

The evaluation also looked at models that are **not** in the catalogue, for their terms: BiRefNet (MIT, trained on DIS5K whose terms forbid commercial use), U²-Net human segmentation (weights terms not stated), SINet (not stated), RMBG-2.0 (non-commercial) and RVM (GPL-3.0). None of them is shipped or downloaded by the module.
