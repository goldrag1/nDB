// GPU compute kernel: strided partial-sum reduction over an f32 column.
//
// Each invocation walks the input with a grid-stride loop, summing every
// `n_threads`-th element into its own partial. The host then sums the
// (small) `partials` array on the CPU. This avoids both f64 atomics
// (unsupported in WGSL) and workgroup shared-memory ceremony, while
// still doing the O(n) work in parallel across the device.
//
// Note: GPUs operate in f32 here, so the result carries f32 precision —
// the CPU reference (`F64Column::sum`) accumulates in f64. Callers that
// need exact f64 totals should use the CPU path; this kernel is for
// throughput on very large columns where f32 is acceptable.

struct Params {
    n: u32,          // number of input elements
    n_threads: u32,  // number of invocations (length of `partials`)
    _pad0: u32,
    _pad1: u32,
};

@group(0) @binding(0) var<storage, read>       input: array<f32>;
@group(0) @binding(1) var<storage, read_write>  partials: array<f32>;
@group(0) @binding(2) var<uniform>              params: Params;

@compute @workgroup_size(64)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let tid = gid.x;
    if (tid >= params.n_threads) {
        return;
    }
    var acc: f32 = 0.0;
    var i: u32 = tid;
    loop {
        if (i >= params.n) { break; }
        acc = acc + input[i];
        i = i + params.n_threads;
    }
    partials[tid] = acc;
}
