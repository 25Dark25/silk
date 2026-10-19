"""Gives the RIFE graph compact inputs and outputs, with identical maths:

- `frames`: uint8 [1, 6, H, W], frame A then frame B, planar RGB. Converted
  in-graph with the same /255 the app did on the CPU (4x less upload).
- `timestep`: float [1], expanded in-graph to the plane the model expects
  (instead of uploading H*W copies of one number).
- `output_gbrp8`: the frame as uint8 planar G, B, R (FFmpeg's gbrp), rounded
  exactly like the app did on the CPU: floor(v * 255 + 0.5) clamped to 0..255.
`output` (float RGB) and `flow` stay available for the motion blur."""
import sys, onnx
import numpy as np
from onnx import helper, numpy_helper, TensorProto

src, dst = sys.argv[1], sys.argv[2]
m = onnx.load(src)
g = m.graph
old = g.input[0]
assert old.name == "input"
dims = old.type.tensor_type.shape.dim
batch, height, width = dims[0].dim_param, dims[2].dim_param, dims[3].dim_param
del g.input[:]
g.input.extend([
    helper.make_tensor_value_info("frames", TensorProto.UINT8, [batch, 6, height, width]),
    helper.make_tensor_value_info("timestep", TensorProto.FLOAT, [1]),
])
c = lambda name, arr: g.initializer.append(numpy_helper.from_array(np.array(arr), name))
c("cio_255", np.float32(255.0)); c("cio_half", np.float32(0.5)); c("cio_zero", np.float32(0.0))
c("cio_ones2", np.array([1, 1], dtype=np.int64)); c("cio_s2", np.array([2], dtype=np.int64))
c("cio_s4", np.array([4], dtype=np.int64)); c("cio_ax0", np.array([0], dtype=np.int64))
c("cio_t_shape", np.array([1, 1, 1, 1], dtype=np.int64)); c("cio_split3", np.array([1, 1, 1], dtype=np.int64))
pre = [
    helper.make_node("Cast", ["frames"], ["cio_f"], to=TensorProto.FLOAT),
    helper.make_node("Div", ["cio_f", "cio_255"], ["cio_rgb"]),
    helper.make_node("Shape", ["frames"], ["cio_shape"]),
    helper.make_node("Slice", ["cio_shape", "cio_s2", "cio_s4", "cio_ax0"], ["cio_hw"]),
    helper.make_node("Concat", ["cio_ones2", "cio_hw"], ["cio_plane_shape"], axis=0),
    helper.make_node("Reshape", ["timestep", "cio_t_shape"], ["cio_t"]),
    helper.make_node("Expand", ["cio_t", "cio_plane_shape"], ["cio_tplane"]),
    helper.make_node("Concat", ["cio_rgb", "cio_tplane"], ["input"], axis=1),
]
post = [
    helper.make_node("Mul", ["output", "cio_255"], ["cio_scaled"]),
    helper.make_node("Add", ["cio_scaled", "cio_half"], ["cio_shifted"]),
    helper.make_node("Floor", ["cio_shifted"], ["cio_floor"]),
    helper.make_node("Clip", ["cio_floor", "cio_zero", "cio_255"], ["cio_clipped"]),
    helper.make_node("Cast", ["cio_clipped"], ["cio_u8"], to=TensorProto.UINT8),
    helper.make_node("Split", ["cio_u8", "cio_split3"], ["cio_r", "cio_g", "cio_b"], axis=1),
    helper.make_node("Concat", ["cio_g", "cio_b", "cio_r"], ["output_gbrp8"], axis=1),
]
nodes = pre + list(g.node) + post
del g.node[:]
g.node.extend(nodes)
g.output.append(helper.make_tensor_value_info("output_gbrp8", TensorProto.UINT8, ["o_batch", 3, "o_height", "o_width"]))
onnx.checker.check_model(m)
onnx.save(m, dst)
print("inputs", [i.name for i in g.input], "outputs", [o.name for o in g.output])
