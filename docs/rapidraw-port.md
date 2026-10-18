# RapidRAW ideas in LightCraft (fork plan and measurements)

This fork ports editing-speed ideas from [RapidRAW](https://github.com/CyberTimon/RapidRAW) into
LightCraft's own GPU pipeline ([`gpu-pipeline.md`](gpu-pipeline.md)). RapidRAW is AGPL-3.0, so only
ideas come across: every line here is written fresh against LightCraft's code, and no RapidRAW
source, shader or asset is copied (credit: `NOTICE`, `README.md`).

*Last updated: 2026-10-18.*

## Rules for every speed change

- **Same pixels.** The CPU pipeline stays the reference. A GPU change keeps
  `crates/gpu/tests/equivalence.rs` inside its bounds (mean |Δ| < 0.5, max ≤ 3 LSB; measured max 1
  LSB, i.e. within 1/255) and adds cases for what it touches.
- **Faster, measured.** It beats the Phase 1 baseline below on the scenario it targets, measured
  with `cargo xtask bench` (or a new `render_bench` scenario added in the same change), before and
  after, on the same machine.
- **Never crash.** Anything the fast path cannot do falls back to today's path, as the GPU renderer
  already does (`Fallback`).

## Phases

| phase | idea (from RapidRAW) | LightCraft today | status |
|---|---|---|---|
| 1 | — | fork, build, baseline | this document |
| 2 | draw the loupe straight from GPU memory | every render read back, histogram on the CPU, re-uploaded to egui | open item in `gpu-pipeline.md` |
| 3 | render only the visible region and a downscaled preview while dragging; half-float trial | stage cache, draft quality, zoom windows | — |
| 4 | GPU thumbnails for edited photos | thumbnails on the CPU | — |
| 5 | tiled GPU exports (overlap = largest filter radius) | exports over the buffer limit render on the CPU | open item in `gpu-pipeline.md` |

Phases 6 (Edit In ▸ Topaz) and 7 (smart culling) are features, not ports, and follow the same
repository rules.

## Phase 1 baseline

### Cloud container (CPU only), 2026-10-18

`cargo xtask bench` at upstream `a945533`, 4 cores, no GPU, procedural 24 MP source (the raw corpus
host was not reachable from there). Minimum of 5 runs; wall-clock and process CPU time.

| scenario | wall | CPU |
|---|---|---|
| loupe 1920×1280 cold | 326.6 ms | 1068.5 ms |
| loupe draft 1152×768 cold | 103.7 ms | 335.2 ms |
| loupe 1920×1280 exposure drag (warm) | 84.0 ms | 320.3 ms |
| loupe draft highlights drag (warm) | 32.6 ms | 123.2 ms |
| loupe draft clarity drag (warm) | 32.8 ms | 123.9 ms |
| loupe draft NR drag (warm) | 66.9 ms | 216.4 ms |
| loupe 1920×1280 NR drag (warm) | 248.6 ms | 819.6 ms |
| window 3072×2304 first (upload original) | 1004.2 ms | 3263.3 ms |
| window pan (new window, warm source) | 968.0 ms | 3059.1 ms |
| window exposure change (warm) | 253.4 ms | 978.0 ms |
| window draft exposure drag (warm) | 251.7 ms | 957.6 ms |
| window NR change (warm) | 818.6 ms | 2798.8 ms |
| finish 1920×1280 typical | 88.9 ms | 336.0 ms |
| export render 6000×4000 | 4774.5 ms | 15458.4 ms |
| export JPEG encode | 404.8 ms | 839.3 ms |

These are the CPU fallback's numbers. The ports target the GPU path, so the baseline that counts is
the GPU column on a machine with a GPU.

### Your machine (GPU)

```sh
cargo xtask corpus --download   # CC0 raws from raw.pixls.us into corpus/ (sha256-checked)
cargo xtask bench               # CPU and GPU columns; appends to target/bench/history.jsonl
LIGHTCRAFT_PROFILE=1 cargo run --release -p lightcraft-engine --example render_bench \
  -- corpus/raw/arw-sony-a7m3-compressed.arw   # per-stage timings, incl. "finish + readback"
```

Keep `target/bench/history.jsonl`: each later run compares itself with the previous one and flags
CPU-time regressions over 20 %. Upstream's own GPU figures (Apple M4 Pro) are in
[`gpu-pipeline.md` → Measurements](gpu-pipeline.md#measurements).

## Testing the GPU path without a GPU

`LIGHTCRAFT_GPU_SOFTWARE=1` lets the compute device use a software adapter (Mesa lavapipe on Linux:
`apt install mesa-vulkan-drivers`). It is far slower than the CPU pipeline, so it is for correctness
only; timings from it mean nothing. On lavapipe (llvmpipe, LLVM 20) every `lightcraft-gpu` test
passes, the equivalence cases at max 1 LSB:

```sh
LIGHTCRAFT_GPU_SOFTWARE=1 cargo test -p lightcraft-gpu --release
```

lavapipe binds at most 128 MiB per storage buffer, so 24 MP sources are resampled on the CPU there
and the test that counts their uploads skips.
