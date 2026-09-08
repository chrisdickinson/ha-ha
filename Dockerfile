# Built by .github/workflows/release.yml from prebuilt linux binaries.
# Expects dist/${TARGETARCH}/ha-ha to exist (amd64 and arm64).
#
# This image ships NO language servers. `ha-ha extract` shells out to
# rust-analyzer / gopls / tsc / pyright-langserver / metals, so use this as a
# base layer to add the servers you need, or mount a project and run the
# subcommands that don't need one (e.g. `validate`).
FROM debian:trixie-slim

ARG TARGETARCH

COPY dist/${TARGETARCH}/ha-ha /usr/local/bin/ha-ha
RUN chmod +x /usr/local/bin/ha-ha

WORKDIR /work

ENTRYPOINT ["/usr/local/bin/ha-ha"]
