# MES-Intelligence edge image: the edge agent and the simulated PLC (one image, two programs).
#
#   docker run mes-edge edge-agent --cell 1000/1000/assy/l1/press3 --plc plc:5020 --mqtt-url mqtt://broker:1883
#   docker run mes-edge plc-sim --listen 0.0.0.0:5020
#
# Stage 1 compiles with the full Rust toolchain; stage 2 keeps only the two binaries.
FROM rust:1.85-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked

FROM debian:bookworm-slim
RUN useradd --create-home edge && mkdir /data && chown edge /data
COPY --from=build /src/target/release/edge-agent /src/target/release/plc-sim /usr/local/bin/
USER edge
ENV EDGE_STATE_FILE=/data/edge-state.txt
CMD ["edge-agent"]
