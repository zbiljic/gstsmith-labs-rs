"""Regenerate synthetic contract fixtures; run the fixtures:segment mise task."""
from pathlib import Path

import numpy as np
import onnx
from onnx import TensorProto as T, helper as h, numpy_helper

ROOT = Path(__file__).resolve().parent


def save(name, inputs, outputs):
    nodes, values = [], []
    for label, array in outputs:
        array = np.asarray(array)
        nodes.append(h.make_node("Constant", [], [label], value=numpy_helper.from_array(array)))
        dtype = T.INT64 if array.dtype == np.int64 else T.FLOAT
        values.append(h.make_tensor_value_info(label, dtype, list(array.shape)))
    graph = h.make_graph(nodes, name, [h.make_tensor_value_info(*i) for i in inputs], values)
    model = h.make_model(graph, opset_imports=[h.make_opsetid("", 18)], ir_version=9)
    onnx.checker.check_model(model, full_check=True)
    assert all(o.type.tensor_type.shape.dim for o in model.graph.output)
    onnx.save(model, ROOT / f"{name}.onnx")


def f(data, shape):
    return np.asarray(data, np.float32).reshape(shape)


image = [("images", T.FLOAT, [1, 3, 4, 4])]
embedding = f([0] * 4, [1, 4, 1, 1])
save("encoder", image, [("embeddings", embedding)])
save("hq-encoder", image, [("embeddings", embedding), ("intermediate", f([0] * 4, [1, 1, 1, 4]))])
save("hierarchical-encoder", image, [(f"embedding{i}", embedding) for i in range(3)])

geometry = [("point_coords", T.FLOAT, [1, "points", 2]), ("point_labels", T.FLOAT, [1, "points"])]
mask_inputs = [("mask_input", T.FLOAT, [1, 1, 1, 1]), ("has_mask_input", T.FLOAT, [1]), ("orig_im_size", T.FLOAT, [2])]
scores = f([0.2, 0.9, 0.4], [1, 3])
# The best-quality mask is left foreground, right background. Other masks disagree.
masks = f([-5] * 4 + [5, -5, 5, -5] + [5] * 4, [1, 3, 2, 2])
outputs = [("masks", masks), ("iou_predictions", scores), ("low_res_masks", masks)]
first = [("image_embeddings", T.FLOAT, [1, 4, 1, 1])]
save("sam-decoder", first + geometry + mask_inputs, outputs)
save("hq-decoder", first + [("intermediate", T.FLOAT, [1, 1, 1, 1, 4])] + geometry + mask_inputs, outputs)
save("edge-decoder", first + geometry, [("scores", scores), ("masks", masks)])
save("sam2-decoder", [(f"embedding{i}", T.FLOAT, [1, 4, 1, 1]) for i in range(3)] + geometry + mask_inputs, outputs[:2])
save("sam3-decoder", [("input_points", T.FLOAT, [1, 1, "points", 2]), ("input_labels", T.INT64, [1, 1, "points"]), ("input_boxes", T.FLOAT, [1, "boxes", 4])] + [(f"embedding{i}", T.FLOAT, [1, 4, 1, 1]) for i in range(3)], [("iou_scores", scores.reshape(1, 1, 3)), ("pred_masks", masks.reshape(1, 1, 3, 2, 2)), ("object_score_logits", f([5], [1, 1, 1]))])
save("birefnet", image, [("mask", f([5, -5, 5, -5], [1, 1, 2, 2]))])
save("ecdetseg", image + [("orig_target_sizes", T.INT64, [1, 2])], [("labels", np.array([[0, 2]], np.int64)), ("boxes", f([0, 0, 4, 4] * 2, [1, 2, 4])), ("scores", f([0.9, 0.8], [1, 2])), ("masks", f([5, -5, 5, -5, -5, 5, -5, 5], [1, 2, 2, 2]))])
for name, classes in [("fastsam", 1), ("yoloe", 4585)]:
    predictions = np.zeros((1, 4 + classes + 1, 2), np.float32)
    predictions[0, :4, :] = np.array([[2, 2], [2, 2], [4, 4], [4, 4]], np.float32)
    predictions[0, 4, 0] = 0.9
    predictions[0, 4 + classes - 1, 1] = 0.8
    predictions[0, -1, :] = [1, -1]
    save(name, image, [("output0", predictions), ("output1", f([5, -5, 5, -5], [1, 1, 2, 2]))])
save("yoloe26", image, [("output0", f([0, 0, 4, 4, 0.9, 0, 1, 0, 0, 4, 4, 0.8, 4584, -1], [1, 2, 7])), ("output1", f([5, -5, 5, -5], [1, 1, 2, 2]))])
save("nonfinite", image, [("mask", f([float("nan"), 0, 0, 0], [1, 1, 2, 2]))])
print("Validated and generated segmentation contract fixtures")
