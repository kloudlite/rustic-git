# The Kompress model as an HTTP service. The wire contract below is headroom's own remote
# contract (headroom/transforms/kompress_remote.py) — unchanged, so headroom could point
# HEADROOM_KOMPRESS_ENDPOINT at this service directly.
from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse
from headroom.transforms.kompress_compressor import KompressCompressor

app = FastAPI()
comp = KompressCompressor()


@app.on_event("startup")
def _preload() -> None:
    # The image already proved this offline load works at build time; allow_download=False
    # here is the same guarantee at runtime — no network call is ever attempted.
    comp.preload(allow_download=False)


@app.get("/healthz")
def healthz():
    if comp.is_ready():
        return {"ready": True}
    return JSONResponse(status_code=503, content={"ready": False})


@app.post("/compress")
async def compress_endpoint(request: Request):
    body = await request.json()
    content = body.get("content")
    if not isinstance(content, str):
        return JSONResponse(status_code=400, content={"error": "content must be a string"})
    target_ratio = body.get("target_ratio")
    try:
        r = comp.compress(content, target_ratio=target_ratio, allow_download=False)
    except Exception as e:  # the client fails open on any error
        return JSONResponse(status_code=500, content={"error": str(e)})
    return {
        "compressed": r.compressed,
        "original_tokens": r.original_tokens,
        "compressed_tokens": r.compressed_tokens,
        "compression_ratio": r.compression_ratio,
    }
