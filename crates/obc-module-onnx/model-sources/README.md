# Reproducing the model files

The module's catalogue (`src/catalogue.rs`) names four ONNX files and their SHA-256. Two kinds of them are our own conversions of upstream releases; this directory has what is needed to make them again: the scripts, the commands and the checksums of the inputs and outputs. The weights themselves are never committed. The licences are in [LICENSES.md](LICENSES.md).

## Tool versions

| Tool | Version |
|---|---|
| Python | 3.12 (MediaPipe), 3.10 (PaddlePaddle, which has no wheel for newer Pythons) |
| `tf2onnx` | 1.17.0, with `tensorflow-cpu` 2.21.0, `onnx` 1.23.2, `numpy` 2.5.4, `protobuf` 7.36.2 |
| `paddlepaddle` | 2.6.2 |
| `paddle2onnx` | 1.3.1, with `onnx` 1.23.2 |
| ONNX Runtime (to check the result) | 1.31.1 |

Each in a virtual environment of its own; run the scripts with `python -I`.

## The files

| Catalogue file | Made from | Input SHA-256 | Output SHA-256 |
|---|---|---|---|
| `selfie_segmentation_landscape.onnx` | `selfie_segmentation_landscape.tflite` (MediaPipe) | `a77d03f4659b9f6b6c1f5106947bf40e99d7655094b6527f214ea7d451106edd` | `fa02222275c455638fa1190493b659d25ad31e86f1b6459f17bd501f02cbe43d` |
| `pp_humansegv2_mobile_192x192.onnx` | `human_pp_humansegv2_mobile_192x192_inference_model_with_softmax.zip` (PaddleSeg) | `4e1ef6f06bbf6b054bc33664a0a71df622ee397a0f3e78a22ef7f96cf82a771b` | `2bca11ec1f2a662146147d97523d613f5ef4ebf2a89803a95a0cbabfe2bd70a9` |
| `pp_humansegv2_lite_192x192.onnx` | `human_pp_humansegv2_lite_192x192_inference_model_with_softmax.zip` (PaddleSeg) | `9d86bc9c86c282cc2e92eaa089b96e18d30776c6ca745be18e6251a4e2a00110` | `ca96b4436f1ce6a76059e8902e20a9617f6829fb11f6c6c9e876563e6352e6b4` |
| `modnet_photographic.onnx` | the upstream ONNX export, not converted | (same file) | `07c308cf0fc7e6e8b2065a12ed7fc07e1de8febb7dc7839d7b7f15dd66584df9` |

The upstream locations, as the upstream projects publish them:

- MediaPipe: `https://storage.googleapis.com/mediapipe-assets/selfie_segmentation_landscape.tflite`;
- PP-HumanSeg v2: `https://paddleseg.bj.bcebos.com/dygraph/pp_humanseg_v2/human_pp_humansegv2_mobile_192x192_inference_model_with_softmax.zip` and `..._lite_192x192_...`, listed in PaddleSeg's `contrib/PP-HumanSeg/README.md` (release 2.9);
- MODNet: the ONNX file linked from the repository's README (Google Drive), byte-identical to `https://huggingface.co/Xenova/modnet` (`onnx/model.onnx`).

Check every download against the input checksum above before converting it.

## MediaPipe

`tf2onnx` converts the TFLite file but leaves MediaPipe's custom operator `Convolution2DTransposeBias` as an unknown node; [`scripts/fix_tconv.py`](scripts/fix_tconv.py) replaces each with standard ONNX nodes (NHWC to NCHW, `ConvTranspose` with stride 2 and the same weights and bias, back to NHWC) and checks the model.

```sh
python -I -m tf2onnx.convert --tflite selfie_segmentation_landscape.tflite \
    --output selfie_tf2onnx.onnx --opset 13
python -I scripts/fix_tconv.py selfie_tf2onnx.onnx selfie_segmentation_landscape.onnx
sha256sum selfie_segmentation_landscape.onnx
```

The result keeps the TFLite layout: input `input_1`, `[1, 144, 256, 3]`, RGB in `[0, 1]`; output `segment_back`, `[1, 144, 256, 1]`, the person's alpha. It gives the same matte as the community conversion `onnx-community/mediapipe_selfie_segmentation_landscape` (largest difference 3.8e-5), which is NCHW.

## PP-HumanSeg v2

The Paddle inference models have a dynamic input shape; [`scripts/pp_static.py`](scripts/pp_static.py) re-saves each with the shape fixed at `1x3x192x192`, which `paddle2onnx` needs to export the pooling with a fixed kernel. Then `paddle2onnx` at opset 13.

```sh
unzip human_pp_humansegv2_mobile_192x192_inference_model_with_softmax.zip -d x
python -I scripts/pp_static.py \
    x/human_pp_humansegv2_mobile_192x192_inference_model_with_softmax \
    st/human_pp_humansegv2_mobile_192x192 192 192
paddle2onnx --model_dir st/human_pp_humansegv2_mobile_192x192 \
    --model_filename model.pdmodel --params_filename model.pdiparams \
    --opset_version 13 --save_file pp_humansegv2_mobile_192x192.onnx
sha256sum pp_humansegv2_mobile_192x192.onnx
```

The same for `lite`. If `paddle2onnx` leaves an `AveragePool` whose kernel is `-1` (an adaptive pooling it could not resolve), [`scripts/pp_fix_pool.py`](scripts/pp_fix_pool.py) sets the kernel and stride from the fixed shape: `python -I scripts/pp_fix_pool.py <static model dir> in.onnx out.onnx 192 192`. The evaluation that produced the catalogue's files kept this script with the conversion; it did not record for which of the two files it was needed, so the output checksum above is the reference: a file that matches it is the catalogue's.

The result: input `x`, `[1, 3, 192, 192]`, RGB in `[-1, 1]` (`(v / 255 - 0.5) / 0.5`); output `save_infer_model/scale_0.tmp_0`, `[1, 2, 192, 192]`, a softmax over background and person (the matte is channel 1). The module squashes the frame to 192x192 (it does not keep the aspect ratio), as the evaluation did.

## MODNet

No conversion. Input `input`, `[1, 3, H, W]` with `H` and `W` dynamic, RGB in `[-1, 1]`; the module scales the frame so its short side is 288, both sides rounded to a multiple of 32 (288x512 for 16:9). Output `output`, `[1, 1, H, W]`, the alpha.
