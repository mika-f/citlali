# syntax=docker/dockerfile:1

# Built from source rather than Debian's packages:
# - zlib-ng: ~2x faster PNG encoding (decoding measured the same as Debian's zlib).
# - mozjpeg: JPEG `quant_table`, which the default qualities are calibrated against.
# - libheif >= 1.23.2: older versions make libvips flag AVIF loading untrusted, and Citlali blocks
#   untrusted loaders.
# - libvips itself, to link the above. Everything else comes from Debian.
ARG VIPS_VERSION=8.18.6
ARG LIBHEIF_VERSION=1.23.5
ARG ZLIB_NG_VERSION=2.3.3
ARG MOZJPEG_VERSION=4.1.1

FROM rust:1-slim-trixie AS build
ARG VIPS_VERSION LIBHEIF_VERSION ZLIB_NG_VERSION MOZJPEG_VERSION
RUN apt-get update && apt-get install -y --no-install-recommends \
      build-essential ca-certificates cmake curl meson nasm ninja-build pkg-config xz-utils \
      libaom-dev libdav1d-dev libexif-dev libexpat1-dev libglib2.0-dev libhwy-dev libimagequant-dev \
      liblcms2-dev libspng-dev libwebp-dev \
    && rm -rf /var/lib/apt/lists/*

ENV PREFIX=/opt/vips
ENV PKG_CONFIG_PATH=$PREFIX/lib/pkgconfig
# Our zlib-ng and mozjpeg must shadow Debian's libz.so.1 / libjpeg.so.62, which `00-` guarantees on
# every architecture (the multiarch conf sorts first on arm64 otherwise).
RUN echo $PREFIX/lib > /etc/ld.so.conf.d/00-citlali.conf

WORKDIR /src
RUN curl -fsSL https://github.com/zlib-ng/zlib-ng/archive/refs/tags/$ZLIB_NG_VERSION.tar.gz | tar xz \
    && cmake -S zlib-ng-$ZLIB_NG_VERSION -B zlib-ng -G Ninja -DCMAKE_BUILD_TYPE=Release \
      -DCMAKE_INSTALL_PREFIX=$PREFIX -DCMAKE_INSTALL_LIBDIR=lib \
      -DZLIB_COMPAT=ON -DZLIB_ENABLE_TESTS=OFF -DWITH_GTEST=OFF -DBUILD_SHARED_LIBS=ON \
    && cmake --build zlib-ng --target install

RUN curl -fsSL https://github.com/mozilla/mozjpeg/archive/refs/tags/v$MOZJPEG_VERSION.tar.gz | tar xz \
    && cmake -S mozjpeg-$MOZJPEG_VERSION -B mozjpeg -G Ninja -DCMAKE_BUILD_TYPE=Release \
      -DCMAKE_INSTALL_PREFIX=$PREFIX -DCMAKE_INSTALL_LIBDIR=$PREFIX/lib \
      -DENABLE_STATIC=OFF -DPNG_SUPPORTED=OFF -DWITH_TURBOJPEG=OFF \
    && cmake --build mozjpeg --target install

# AVIF only: aom encodes, dav1d decodes, both linked in (no plugin directory to get wrong).
RUN curl -fsSL https://github.com/strukturag/libheif/releases/download/v$LIBHEIF_VERSION/libheif-$LIBHEIF_VERSION.tar.gz | tar xz \
    && cmake -S libheif-$LIBHEIF_VERSION -B libheif -G Ninja -DCMAKE_BUILD_TYPE=Release \
      -DCMAKE_INSTALL_PREFIX=$PREFIX -DCMAKE_INSTALL_LIBDIR=lib -DENABLE_PLUGIN_LOADING=OFF \
      -DWITH_AOM_ENCODER=ON -DWITH_AOM_DECODER=OFF -DWITH_DAV1D=ON -DWITH_LIBDE265=OFF -DWITH_X265=OFF \
      -DWITH_JPEG_ENCODER=OFF -DWITH_JPEG_DECODER=OFF -DWITH_OpenJPEG_ENCODER=OFF -DWITH_OpenJPEG_DECODER=OFF \
      -DWITH_OPENJPH_ENCODER=OFF -DWITH_OPENJPH_DECODER=OFF -DWITH_FFMPEG_DECODER=OFF \
      -DWITH_EXAMPLES=OFF -DWITH_GDK_PIXBUF=OFF -DBUILD_TESTING=OFF \
    && cmake --build libheif --target install

RUN curl -fsSL https://github.com/libvips/libvips/releases/download/v$VIPS_VERSION/vips-$VIPS_VERSION.tar.xz | tar xJ \
    && meson setup vips vips-$VIPS_VERSION --prefix=$PREFIX --libdir=lib --buildtype=release \
      -Dcplusplus=false -Ddeprecated=false -Dexamples=false -Dintrospection=disabled -Dmodules=disabled \
    && meson install -C vips \
    && ldconfig

WORKDIR /src/citlali
COPY Cargo.toml Cargo.lock build.rs ./
COPY src src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/citlali/target \
    cargo build --release --locked && cp target/release/citlali /usr/local/bin/

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
      libaom3 libdav1d7 libexif12 libexpat1 libglib2.0-0t64 libhwy1t64 libimagequant0 liblcms2-2 \
      libsharpyuv0 libspng0 libwebp7 libwebpdemux2 libwebpmux3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /opt/vips/lib /opt/vips/lib
COPY --from=build /etc/ld.so.conf.d/00-citlali.conf /etc/ld.so.conf.d/
COPY --from=build /usr/local/bin/citlali /usr/local/bin/
RUN ldconfig

USER nobody
EXPOSE 8080
ENTRYPOINT ["citlali"]
