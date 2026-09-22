# Kompress: the sys-1 engine's model-backed text compressor, as its own service. The model
# (chopratejas/kompress-v2-base, int8 ONNX, ~261 MB) is baked into the image at build time —
# the owner's call: no runtime download, no egress needed once the pod is up.
FROM python:3.13-slim AS builder

# [proxy], not [ml]: [ml] pulls in torch (~2 GB on linux x86_64) that the ONNX inference path
# never touches.
RUN pip install --no-cache-dir "headroom-ai[proxy]==0.38.0" "huggingface-hub>=1.5.0,<2.0"
ENV HF_HOME=/opt/hf

# Downloads the tokenizer and onnx/kompress-int8-wo.onnx once, at build.
RUN python -c "from headroom.transforms.kompress_compressor import KompressCompressor as K; K().preload(allow_download=True)"

# Proves the runtime image never needs the network: an offline load that fails here fails the
# build, not a pod at 3am.
RUN HF_HUB_OFFLINE=1 TRANSFORMERS_OFFLINE=1 python -c "\
from headroom.transforms.kompress_compressor import KompressCompressor as K; \
k = K(); k.preload(allow_download=False); assert k.is_ready()"

# Only the int8 weights ship; a stray fp32 copy (~601 MB) would double the image for nothing.
RUN find /opt/hf -iname '*fp32*onnx*' -delete

FROM python:3.13-slim
COPY --from=builder /usr/local/lib/python3.13/site-packages /usr/local/lib/python3.13/site-packages
COPY --from=builder /usr/local/bin /usr/local/bin
COPY --from=builder /opt/hf /opt/hf
ENV HF_HOME=/opt/hf \
    HF_HUB_OFFLINE=1 \
    TRANSFORMERS_OFFLINE=1 \
    PYTHONUNBUFFERED=1 \
    HEADROOM_KOMPRESS_ONNX_INTRA_THREADS=2

WORKDIR /opt/kompress
COPY server.py .

USER 1001:1001
EXPOSE 8787
CMD ["uvicorn", "server:app", "--host", "0.0.0.0", "--port", "8787"]
