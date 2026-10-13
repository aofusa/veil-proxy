"""veil Python バインディングの最小例（デコレータ形式）。

    maturin develop --release   # crates/veil-python で一度
    VEIL_LISTEN=127.0.0.1:8000 python examples/api/python-hello/app.py

    curl localhost:8000/ping
    curl localhost:8000/hello/world?greeting=Hi
    curl -X POST localhost:8000/users -d '{"name":"alice","age":30}'
    open http://localhost:8000/docs
"""

import os

import veil


@veil.get("/ping", offload=False)
def ping():
    """自明に軽いハンドラはイベントループ上で直接実行できる（offload=False）。"""
    return "pong"


@veil.get("/hello/{name}", tags=["greeting"])
def hello(name: str, greeting: str = "Hello", times: int = 1):
    """パスパラメータとクエリパラメータ（注釈で型変換される）。"""
    return {"message": f"{greeting}, {name}", "times": times}


@veil.post(
    "/users",
    status_code=201,
    tags=["users"],
    body_constraints={
        "name": {"type": "string", "minLength": 1, "maxLength": 50, "required": True},
        "age": {"type": "integer", "minimum": 0, "maximum": 120, "required": True},
    },
)
def create_user(body: dict):
    """pydantic 無しでも本文を検証できる。違反は 422。"""
    return {"id": 1, **body}


@veil.get("/items/{item_id}")
def get_item(item_id: int, verbose: bool = False):
    if item_id > 100:
        raise veil.HTTPException(404, "item not found")
    return {"item_id": item_id, "verbose": verbose}


if __name__ == "__main__":
    veil.run(listen_cleartext=os.environ.get("VEIL_LISTEN", "127.0.0.1:8000"))
