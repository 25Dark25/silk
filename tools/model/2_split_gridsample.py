"""Splits every GridSample into one GridSample per (batch item, channel).

DirectML miscomputes GridSample on tensors above ~2^26 elements (RIFE at 4K
warps [2, 7, 2176, 3840]). GridSample treats each batch item and channel
independently, so the split is mathematically identical."""
import sys, onnx
from onnx import helper, numpy_helper, shape_inference
import numpy as np

src, dst = sys.argv[1], sys.argv[2]
m = onnx.load(src)
g = m.graph

# Channel counts from shape inference at a concrete size.
probe = onnx.load(src)
dims = probe.graph.input[0].type.tensor_type.shape.dim
dims[0].dim_value, dims[2].dim_value, dims[3].dim_value = 1, 256, 256
probe = shape_inference.infer_shapes(probe, data_prop=True)
shapes = {v.name: [d.dim_value if d.HasField("dim_value") else None for d in v.type.tensor_type.shape.dim]
          for v in probe.graph.value_info}

ones = numpy_helper.from_array(np.array([1, 1], dtype=np.int64), "gs_split_batch")
g.initializer.append(ones)
new_nodes = []
for n in g.node:
    if n.op_type != "GridSample":
        new_nodes.append(n)
        continue
    batch, channels = shapes[n.input[0]][0], shapes[n.input[0]][1]
    assert batch == 2 and channels, (n.name, shapes[n.input[0]])
    per_channel = f"gs_split_c{channels}"
    if not any(i.name == per_channel for i in g.initializer):
        g.initializer.append(numpy_helper.from_array(np.ones(channels, dtype=np.int64), per_channel))
    p = n.name.strip("/").replace("/", "_")
    xs = [f"{p}_x{b}" for b in range(2)]
    gs = [f"{p}_g{b}" for b in range(2)]
    new_nodes.append(helper.make_node("Split", [n.input[0], "gs_split_batch"], xs, axis=0, name=f"{p}_splitx"))
    new_nodes.append(helper.make_node("Split", [n.input[1], "gs_split_batch"], gs, axis=0, name=f"{p}_splitg"))
    per_batch = []
    for b in range(2):
        cs = [f"{p}_x{b}_c{c}" for c in range(channels)]
        new_nodes.append(helper.make_node("Split", [xs[b], per_channel], cs, axis=1, name=f"{p}_splitc{b}"))
        outs = []
        for c in range(channels):
            o = f"{p}_y{b}_c{c}"
            new_nodes.append(helper.make_node("GridSample", [cs[c], gs[b]], [o], name=f"{p}_gs{b}_{c}",
                                              **{a.name: helper.get_attribute_value(a) for a in n.attribute}))
            outs.append(o)
        y = f"{p}_y{b}"
        new_nodes.append(helper.make_node("Concat", outs, [y], axis=1, name=f"{p}_catc{b}"))
        per_batch.append(y)
    new_nodes.append(helper.make_node("Concat", per_batch, [n.output[0]], axis=0, name=f"{p}_catb"))
    print("split", n.name, "batch", batch, "channels", channels)

del g.node[:]
g.node.extend(new_nodes)
onnx.checker.check_model(m)
onnx.save(m, dst)
print("saved", dst)
