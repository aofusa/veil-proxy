"""veil の Python バインディングで fastapi_app.py と同じ 2 エンドポイントを返す（tools/api-bench/README.md）。

    BENCH_OFFLOAD=1|0  BENCH_THREADS=2  BENCH_LISTEN=127.0.0.1:3000  python veil_app.py
"""

import os

import veil

OFFLOAD = os.environ.get("BENCH_OFFLOAD", "1") == "1"


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
        overrides={"logging.level": "warn"},
        openapi=False,
    )
