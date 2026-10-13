"""FastAPI（uvicorn）で veil_app.py と同じ 2 エンドポイントを返す（tools/api-bench/README.md）。

    uvicorn fastapi_app:app --host 127.0.0.1 --port 3000 --workers 2 --log-level warning --no-access-log
    BENCH_SYNC=1 で async def の代わりに def（スレッドプールで実行）にする。
"""

import os

from fastapi import FastAPI
from fastapi.responses import PlainTextResponse

app = FastAPI()

if os.environ.get("BENCH_SYNC", "0") == "1":

    @app.get("/plaintext", response_class=PlainTextResponse)
    def plaintext():
        return "Hello, World!"

    @app.get("/json/{name}")
    def json_(name: str):
        return {"message": f"Hello, {name}"}

else:

    @app.get("/plaintext", response_class=PlainTextResponse)
    async def plaintext():
        return "Hello, World!"

    @app.get("/json/{name}")
    async def json_(name: str):
        return {"message": f"Hello, {name}"}
