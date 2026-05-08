# azcp + RunAI Model Streamer multi-arch image.
#
# Layers:
#   1. NGC PyTorch (CUDA, NCCL, IB user-space libs, OFED stubs, torchrun)
#   2. azcp single-binary (copied from ghcr.io/edwardsp/azcp builds)
#   3. runai-model-streamer + S3 backend (pip)
#   4. The adapter + test driver
#
# Build:
#   docker build -t ghcr.io/<you>/azcp-runai-adapter:latest \
#       -f examples/runai-adapter/Dockerfile examples/runai-adapter
#
# Or in CI mirror the cluster-image workflow.

ARG PYTORCH_BASE=nvcr.io/nvidia/pytorch:24.10-py3
ARG AZCP_VERSION=v0.3.0

FROM ghcr.io/edwardsp/azcp/azcp-cluster:${AZCP_VERSION} AS azcp-bin

FROM ${PYTORCH_BASE}

ARG AZCP_VERSION
ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
        ca-certificates \
        curl \
        openssh-client \
 && rm -rf /var/lib/apt/lists/*

# azcp CLI: copy the statically-built binary out of the cluster image.
# The cluster image is amd64+arm64 multi-arch; this picks the matching one.
COPY --from=azcp-bin /usr/local/bin/azcp /usr/local/bin/azcp
RUN chmod +x /usr/local/bin/azcp && azcp --version

# RunAI Model Streamer + the Azure backend if/when it ships.
# As of 0.13.x the Azure backend is in flight; pin the exact version you tested.
RUN pip install --no-cache-dir \
        "runai-model-streamer>=0.13.0,<0.16.0"

WORKDIR /opt/runai-adapter
COPY azcp_runai_loader.py test_load.py ./

ENV PYTHONPATH=/opt/runai-adapter:${PYTHONPATH:-}
ENV NCCL_DEBUG=WARN \
    NCCL_IB_DISABLE=0 \
    NCCL_SOCKET_IFNAME=eth0 \
    OMP_NUM_THREADS=8

ENTRYPOINT ["torchrun"]
