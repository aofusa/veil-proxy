"""A veil-api application with the recommended production settings (``config.toml``; F-274).

    APP_CONFIG=config.toml python app.py
"""

import asyncio
import os

import veil

HERE = os.path.dirname(os.path.abspath(__file__))


# 何もしない軽いハンドラはワーカーで直接（offload=False）
@veil.get("/healthz", offload=False)
def healthz():
    return "ok"


@veil.get("/api/items/{item_id}")
def item(item_id: int):
    return {"id": item_id, "name": "item %d" % item_id}


@veil.get("/api/items")
def items():
    return {"items": [{"id": i, "name": "item %d" % i} for i in range(50)]}


# [api] max_body_size（ここでは 1 MiB）を超える本文はハンドラの前に 413
@veil.post("/api/items", status_code=201)
def create(req: veil.Request):
    return {"received": len(req.body)}


# I/O を待つハンドラは async def（常駐のイベントループで待つ。スレッドを占有しない）
@veil.get("/api/slow")
async def slow():
    await asyncio.sleep(0.01)
    return {"ok": True}


# CPU の重い処理はハンドラ用プールのスレッドへ
@veil.get("/api/hash")
async def hash_():
    import hashlib

    digest = await veil.run_in_threadpool(lambda: hashlib.sha256(b"x" * 1_000_000).hexdigest())
    return {"sha256": digest}


if __name__ == "__main__":
    veil.run(config_path=os.environ.get("APP_CONFIG", os.path.join(HERE, "config.toml")))
