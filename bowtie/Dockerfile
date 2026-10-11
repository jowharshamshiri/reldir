# The reldir connector, as Bowtie builds implementations: from the source at
# the repository root (`docker build -f bowtie/Dockerfile .`).
FROM rust:1.89-slim AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p reldir-bowtie

FROM debian:bookworm-slim
COPY --from=build /src/target/release/reldir-bowtie /usr/local/bin/reldir-bowtie
CMD ["/usr/local/bin/reldir-bowtie"]
