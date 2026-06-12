//! Optional GPU compute kernel for global numeric reductions
//! (`--features gpu`).
//!
//! This is the GPU counterpart to [`F64Column::sum`](crate::F64Column).
//! It uploads the column to device memory in one transfer, runs the
//! [`sum_reduce.wgsl`](../sum_reduce.wgsl) compute shader to produce a
//! small array of partial sums in parallel, copies those partials back,
//! and finishes the reduction on the host.
//!
//! Honest scope (see the design notes / the CPU-vs-GPU discussion):
//!
//! - It works on `f32`, so the total carries f32 precision — the CPU
//!   path stays the source of truth for exact `f64` sums.
//! - The win only materialises for very large columns, because the
//!   SSD→RAM→VRAM copy (PCIe) has to be amortised. For nDB's measured
//!   workloads the scan is decode-bound, not arithmetic-bound, so this
//!   is provided as an opt-in tool, not the default.
//! - [`gpu_sum`] returns `None` when no GPU adapter is present (e.g. CI /
//!   headless boxes); callers fall back to the CPU reduction. Nothing in
//!   the default build depends on a GPU existing.
#![allow(
    clippy::cast_possible_truncation, // f64 -> f32 is the whole point; column lengths fit u32
    clippy::cast_precision_loss
)]

use crate::batch::F64Column;

use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

/// Push-constant-free uniform params for the kernel. Padded to 16 bytes
/// to satisfy WGSL uniform-buffer alignment.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Params {
    n: u32,
    n_threads: u32,
    _pad0: u32,
    _pad1: u32,
}

/// Number of parallel partial sums the kernel produces. Kept small so
/// the readback + host finish is cheap; the grid-stride loop means each
/// thread still covers `n / N_THREADS` elements.
const N_THREADS: u32 = 1024;
const WORKGROUP_SIZE: u32 = 64;

/// True iff a usable GPU adapter can be acquired right now. Cheap probe
/// for callers that want to decide a dispatch strategy up front.
#[must_use]
pub fn is_available() -> bool {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default())).is_some()
}

/// Sum the valid elements of `col` on the GPU, or return `None` if no GPU
/// adapter is available (the caller should then use [`F64Column::sum`]).
///
/// Null slots are stored as `0.0` in [`F64Column`], so the raw buffer can
/// be reduced directly — the validity mask never has to reach the device.
#[must_use]
pub fn gpu_sum(col: &F64Column) -> Option<f64> {
    if col.is_empty() {
        return Some(0.0);
    }
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
    let adapter =
        pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions::default()))?;
    let (device, queue) = pollster::block_on(adapter.request_device(
        &wgpu::DeviceDescriptor {
            label: Some("ndb-gpu-sum"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            memory_hints: wgpu::MemoryHints::Performance,
        },
        None,
    ))
    .ok()?;

    Some(run_reduction(&device, &queue, &col.data))
}

fn run_reduction(device: &wgpu::Device, queue: &wgpu::Queue, data: &[f64]) -> f64 {
    // GPUs reduce in f32; the f64 source narrows on upload.
    let input_f32: Vec<f32> = data.iter().map(|&x| x as f32).collect();
    let n = input_f32.len() as u32;
    let n_threads = N_THREADS.min(n.max(1));

    let input_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("input"),
        contents: bytemuck::cast_slice(&input_f32),
        usage: wgpu::BufferUsages::STORAGE,
    });
    let partials_size = u64::from(n_threads) * std::mem::size_of::<f32>() as u64;
    let partials_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("partials"),
        size: partials_size,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let staging_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("staging"),
        size: partials_size,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let params = Params {
        n,
        n_threads,
        _pad0: 0,
        _pad1: 0,
    };
    let params_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("params"),
        contents: bytemuck::bytes_of(&params),
        usage: wgpu::BufferUsages::UNIFORM,
    });

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("sum_reduce"),
        source: wgpu::ShaderSource::Wgsl(include_str!("sum_reduce.wgsl").into()),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("sum_reduce"),
        layout: None,
        module: &shader,
        entry_point: "main",
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });

    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("sum_reduce"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: input_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: partials_buf.as_entire_binding(),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: params_buf.as_entire_binding(),
            },
        ],
    });

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("sum_reduce"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("sum_reduce"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        let workgroups = n_threads.div_ceil(WORKGROUP_SIZE);
        pass.dispatch_workgroups(workgroups, 1, 1);
    }
    encoder.copy_buffer_to_buffer(&partials_buf, 0, &staging_buf, 0, partials_size);
    queue.submit(Some(encoder.finish()));

    // Map the staging buffer and finish the reduction on the host.
    let slice = staging_buf.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        let _ = tx.send(res);
    });
    device.poll(wgpu::Maintain::Wait);
    rx.recv().ok();
    let partials: Vec<f32> = {
        let view = slice.get_mapped_range();
        bytemuck::cast_slice::<u8, f32>(&view).to_vec()
    };
    staging_buf.unmap();

    partials.iter().map(|&x| f64::from(x)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_sum_matches_cpu_within_f32_tolerance_or_skips() {
        let col = F64Column {
            data: (0..10_000).map(|i| f64::from(i) * 0.5).collect(),
            valid: vec![true; 10_000],
        };
        match gpu_sum(&col) {
            None => eprintln!("no GPU adapter — skipping (CPU fallback path is the default)"),
            Some(got) => {
                let want = col.sum();
                // f32 accumulation drifts; allow a relative tolerance.
                let rel = (got - want).abs() / want.abs().max(1.0);
                assert!(rel < 1e-3, "gpu={got} cpu={want} rel={rel}");
            }
        }
    }

    #[test]
    fn empty_column_sums_to_zero_without_adapter() {
        assert_eq!(gpu_sum(&F64Column::default()), Some(0.0));
    }
}
