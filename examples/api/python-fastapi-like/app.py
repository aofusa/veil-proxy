"""FastAPI 風の使い方（App インスタンス・pydantic モデル・HTTPException・Request）。

pydantic は任意依存: 入っていなければ body_constraints による検証にフォールバックする。

    VEIL_LISTEN=127.0.0.1:8000 python examples/api/python-fastapi-like/app.py
"""

import os

import veil
from veil import App, HTTPException, Request

try:
    from pydantic import BaseModel, Field

    class Item(BaseModel):
        name: str = Field(min_length=1, max_length=50)
        price: float = Field(ge=0)
        tags: list = []

    HAVE_PYDANTIC = True
except ImportError:  # pydantic 無し
    Item = None
    HAVE_PYDANTIC = False

app = App(title="Items API", version="1.0.0")
DB = {}

if HAVE_PYDANTIC:

    @app.post("/items", status_code=201, tags=["items"])
    def create_item(item: Item):
        """アイテムを作る（pydantic で検証。違反は FastAPI 形の 422）。"""
        DB[item.name] = item
        return item

else:

    @app.post(
        "/items",
        status_code=201,
        tags=["items"],
        body_constraints={
            "name": {"type": "string", "minLength": 1, "maxLength": 50, "required": True},
            "price": {"type": "number", "minimum": 0, "required": True},
        },
    )
    def create_item(body: dict):
        """アイテムを作る（body_constraints で検証。違反は FastAPI 形の 422）。"""
        DB[body["name"]] = body
        return body


@app.get("/items/{name}", tags=["items"])
def read_item(name: str):
    """名前で取得。無ければ 404。"""
    if name not in DB:
        raise HTTPException(404, "Item not found")
    return DB[name]


@app.get("/items", tags=["items"])
def list_items(limit: int = 10, q: str = ""):
    return [k for k in DB if q in k][:limit]


@app.get("/whoami")
def whoami(request: Request):
    """Request から生の情報を読む。"""
    return {
        "method": request.method,
        "path": request.path,
        "client_ip": request.client_ip,
        "user_agent": request.headers.get("user-agent"),
        "query": request.query_params,
    }


@app.get("/pydantic")
def has_pydantic():
    return {"pydantic": HAVE_PYDANTIC}


if __name__ == "__main__":
    app.run(listen=os.environ.get("VEIL_LISTEN", "127.0.0.1:8000"))
