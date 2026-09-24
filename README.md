# Citlali

An image optimization server. POST an image with query parameters and get back a resized, re-encoded image.
Built on [libvips](https://www.libvips.org/). Citlali keeps no cache; put a CDN or cache in front of it.

## Requirements

- Rust 1.85+ (edition 2024)
- libvips 8.15+ with AVIF support (libheif + an AV1 encoder), discoverable via `pkg-config`
  - macOS: `brew install vips pkgconf`
  - Debian/Ubuntu: `apt install libvips-dev pkg-config` (plus `libheif-plugin-aomenc` for AVIF output).
    AVIF *input* needs libheif 1.23.2+; libvips treats older libheif as untrusted and Citlali refuses it.
    The Docker image below handles all of this.

## Run

```sh
cargo run --release
```

Or with Docker:

```sh
docker build -t citlali .
docker run --rm -p 8080:8080 citlali
```

The Dockerfile builds for both `linux/arm64` and `linux/amd64`
(`docker buildx build --platform linux/amd64 ...`); every codec picks its SIMD path (NEON, SSE4.2/AVX2/AVX-512)
at run time, so no architecture-specific build is needed.

The image builds libvips 8.18 from source with zlib-ng (about 2x faster PNG encoding), mozjpeg
(which the JPEG default quality is calibrated against) and libheif 1.23 (AVIF). It runs as `nobody`
and drains in-flight requests on `docker stop`.

| Env                      | Default          | Meaning                                    |
| ------------------------ | ---------------- | ------------------------------------------ |
| `CITLALI_LISTEN`         | `0.0.0.0:8080`   | Listen address                             |
| `CITLALI_MAX_BODY_BYTES` | `33554432`       | Max request body (32 MiB)                  |
| `CITLALI_CONCURRENCY`    | number of CPUs   | Images processed at once; the rest queue   |
| `CITLALI_QUEUE_TIMEOUT_SECS` | `30`         | Max wait in the queue before answering `503` |
| `CITLALI_MAX_INPUT_PIXELS` | `100000000`    | Larger inputs get `413` before decoding     |
| `CITLALI_AVIF_SPEED`     | `9`              | AVIF encoder speed, 0 (slowest, smallest) – 9 (fastest, largest) |

`CITLALI_AVIF_SPEED` follows libaom / avifenc semantics and maps to libvips `effort = 9 - speed`.
AVIF encoding usually dominates request time. Measured on a Neoverse-N1 core (bench images at 1280 px,
same perceptual quality): speed 6 takes 1.09 s and 84 KB per image, speed 9 takes 0.22 s and 119 KB —
still smaller than WebP (0.22 s, 135 KB). Avoid speed 8: libaom produces larger files there than at 9.

### Memory sizing

Peak memory is dominated by AVIF *input*, which cannot be decoded at reduced size: about 9 MiB per megapixel
(20 MP → 190 MiB, 100 MP → 880 MiB). JPEG input shrinks on load and stays under 100 MiB even at 36 MP; PNG and
WebP land in between. Budget `CITLALI_CONCURRENCY × peak` below the container's memory limit, and lower
`CITLALI_MAX_INPUT_PIXELS` on small instances. For Cloudflare Containers (`linux/amd64`, no swap: running out of
memory restarts the instance):

| Instance type | vCPU | Memory | `CITLALI_CONCURRENCY` | `CITLALI_MAX_INPUT_PIXELS` |
| ------------- | ---- | ------ | --------------------- | -------------------------- |
| lite          | 1/16 | 256 MiB | 1                    | `20000000`                 |
| basic         | 1/4  | 1 GiB  | 1                     | `100000000` (default)      |
| standard-1    | 1/2  | 4 GiB  | 1                     | default                    |
| standard-2    | 1    | 6 GiB  | 1                     | default                    |
| standard-3    | 2    | 8 GiB  | 2                     | default                    |
| standard-4    | 4    | 12 GiB | 4                     | default                    |

Cloudflare routes stateless container requests randomly without looking at load, so a short
`CITLALI_QUEUE_TIMEOUT_SECS` (a few seconds) plus a retry on `503` in the fronting Worker spreads bursts better
than a long queue on one instance.

## API

### `POST /transform`

The request body is the image (JPEG, PNG, WebP or AVIF, detected by magic bytes).

```sh
curl --data-binary @photo.jpg -o out.webp \
  'http://localhost:8080/transform?width=800&height=600&fit=cover&format=webp&quality=75'
```

| Param     | Values                                  | Default          |
| --------- | --------------------------------------- | ---------------- |
| `width`   | 1–8192                                  | unbounded        |
| `height`  | 1–8192                                  | unbounded        |
| `fit`     | `scale-down`, `cover`, `crop`, `blur`   | `scale-down`     |
| `gravity` | `centre` (`center`), `face`             | `centre`         |
| `format`  | `jpeg` (`jpg`), `png`, `webp`, `avif`   | same as input    |
| `quality` | 1–100 (encoder-native scale)            | JPEG 78, WebP 83, AVIF 62–69 (by speed), PNG lossless |

- `scale-down`: fit inside `width`×`height` keeping aspect ratio; never enlarges.
- `cover`: fill `width`×`height` exactly (enlarging if needed) and centre-crop the overflow.
- `crop`: like `cover` but never enlarges; an axis smaller than the box stays as is.
- `blur`: fit inside the box without enlarging, over a blurred copy of the image filling the whole box.
- `gravity` picks what `cover` and `crop` keep in frame. `face` looks for anime-style faces (illustrations,
  VRChat-style avatars) and centres the crop on the most confident one, as far as the image edges allow; with no
  face found it falls back to `centre`. Faces smaller than about 5% of the image's long edge are missed.
  Detection adds a small decode of the input (~40 ms on the bench images).
- `cover` / `crop` / `blur` need both `width` and `height`; with only one they behave like `scale-down`.
- For PNG, an explicit `quality` switches to palette quantisation (lossy, much smaller).
- `quality` is passed to the encoder as is, and each encoder's scale differs. The defaults are calibrated to
  the same perceptual quality (SSIMULACRA2 ≈ 73, i.e. mozjpeg Q80) on `bench/images`, using the Docker
  image's codecs.
- EXIF orientation is applied, colours are converted to sRGB and all metadata is stripped.
- Inputs above `CITLALI_MAX_INPUT_PIXELS` (100 megapixels by default) are rejected before decoding.

Errors: `400` bad parameter, `413` body or pixel count too large, `415` unsupported format, `422` undecodable image,
`503` queue wait exceeded `CITLALI_QUEUE_TIMEOUT_SECS`.

"Number of CPUs" honours container CPU limits (cgroup quota), rounded down with a minimum of 1: both `512m`
and `1024m` give 1. libvips still spreads each image over the node's cores, so 1 already keeps a ~1-core limit busy.

### `GET /metrics`

Prometheus text format:

- `citlali_requests_total{status}`
- `citlali_transform_duration_seconds{format}` (histogram, successful requests, includes queueing)
- `citlali_input_bytes_total`, `citlali_output_bytes_total`
- `citlali_vips_tracked_memory_bytes`
- `citlali_process_*` (Linux only): CPU seconds, resident memory, threads, open fds.
  CPU is reported in whole seconds; for per-request figures use the container's cgroup/cAdvisor counters.

### `GET /healthz`

Returns `ok`.

## Benchmark

Needs [oha](https://github.com/hatoo/oha) and `jq`.

```sh
cargo run --release &
bench/bench.sh photo.jpg                            # default scenario set
bench/bench.sh bench/images                         # every image in a directory
bench/bench.sh photo.jpg 'width=640&format=avif'    # custom queries
DURATION=30s CONNECTIONS=32 URL=http://host:8080 bench/bench.sh photo.jpg
```

Prints req/s, p50/p99 latency, output size and success rate for each image and query.
On laptops, thermal throttling skews req/s between consecutive runs; compare CPU time per request when tuning.

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.

`gravity=face` uses [lbpcascade_animeface](https://github.com/nagadomi/lbpcascade_animeface) by nagadomi
(MIT, see the header of `src/lbpcascade_animeface.xml`).
