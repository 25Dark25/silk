"""Adds a `flow` output to RIFE v4.25 (vs-mlrt "rife_v2" export).

`/Add_7_output_0` is the sum of the flows of RIFE's five blocks: for each
output pixel, its displacement to frame A (channels 0-1) and to frame B
(channels 2-3), in pixels of the internally padded frame. The motion blur
reads it; the image output is unchanged.

Usage: python 1_add_flow.py rife_v2/rife_v4.25.onnx step1.onnx
"""
import sys, onnx
from onnx import helper, TensorProto
src, dst = sys.argv[1], sys.argv[2]
m = onnx.load(src)
g = m.graph
assert any(o == "/Add_7_output_0" for n in g.node for o in n.output)
g.node.append(helper.make_node("Identity", ["/Add_7_output_0"], ["flow"], name="flow_out"))
g.output.append(helper.make_tensor_value_info("flow", TensorProto.FLOAT, ["flow_batch", 4, "flow_height", "flow_width"]))
onnx.checker.check_model(m)
onnx.save(m, dst)
print("ok", [o.name for o in m.graph.output])
