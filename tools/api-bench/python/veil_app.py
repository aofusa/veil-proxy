"""veil の Python バインディングで fastapi_app.py と同じ 2 エンドポイントを返す（tools/api-bench/README.md）。

    BENCH_OFFLOAD=1|0  BENCH_ASYNC=0|1  BENCH_THREADS=2  BENCH_LISTEN=127.0.0.1:3000  BENCH_ACCESS_LOG=1|0  python veil_app.py
"""

import os

import veil

OFFLOAD = os.environ.get("BENCH_OFFLOAD", "1") == "1"
ASYNC = os.environ.get("BENCH_ASYNC", "0") == "1"  # F-299: `async def`（永続のイベントループの経路）

if ASYNC:

    @veil.get("/plaintext")
    async def plaintext():
        return "Hello, World!"

    @veil.get("/json/{name}")
    async def json_(name: str):
        return {"message": f"Hello, {name}"}

else:

    @veil.get("/plaintext", offload=OFFLOAD)
    def plaintext():
        return "Hello, World!"

    @veil.get("/json/{name}", offload=OFFLOAD)
    def json_(name: str):
        return {"message": f"Hello, {name}"}


if __name__ == "__main__":
    veil.run(
        listen_cleartext=os.environ.get("BENCH_LISTEN", "127.0.0.1:3000"),
        threads=int(os.environ.get("BENCH_THREADS", "2")),
        overrides={
            "logging.level": "warn",
            "access_log.enabled": os.environ.get("BENCH_ACCESS_LOG", "1") == "1",
        },
        openapi=False,
    )
