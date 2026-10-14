"""GraphQL（strawberry）を veil の Python の普通のハンドラで載せる（F-210）。veil は GraphQL の実装に依存しない
ので、graphene・ariadne 等でも同じ形で書ける。

    pip install strawberry-graphql
    VEIL_LISTEN=127.0.0.1:8000 python app.py
    curl -X POST localhost:8000/graphql -H 'content-type: application/json' -d '{"query":"{ add(a: 1, b: 2) }"}'
    curl -N -X POST localhost:8000/graphql -H 'accept: text/event-stream' -H 'content-type: application/json' \
         -d '{"query":"subscription { countdown(start: 3) }"}'
"""

import asyncio
import os
from typing import AsyncGenerator

import strawberry

import veil


@strawberry.type
class Query:
    @strawberry.field
    def add(self, a: int, b: int) -> int:
        return a + b

    @strawberry.field
    def greeting(self, name: str) -> str:
        return "Hello, %s!" % name


COUNTER = {"n": 0}


@strawberry.type
class Mutation:
    @strawberry.mutation
    def increment(self, by: int) -> int:
        COUNTER["n"] += by
        return COUNTER["n"]


@strawberry.type
class Subscription:
    @strawberry.subscription
    async def countdown(self, start: int) -> AsyncGenerator[int, None]:
        for n in range(start, -1, -1):
            await asyncio.sleep(0.01)
            yield n


schema = strawberry.Schema(query=Query, mutation=Mutation, subscription=Subscription)


def _result(r):
    out = {"data": r.data}
    if r.errors:
        out["errors"] = [{"message": e.message} for e in r.errors]
    return out


@veil.route("/graphql", methods=["GET", "POST"])
def graphql(req: veil.Request):
    if req.method == "GET":
        query, variables = req.query_params.get("query", ""), None
    else:
        body = req.json()
        query, variables = body.get("query", ""), body.get("variables")
    if "text/event-stream" in (req.headers.get("accept") or ""):
        # サブスクリプション（GraphQL over SSE の distinct connections モード）
        async def events():
            agen = await schema.subscribe(query, variable_values=variables)
            async for r in agen:
                yield {"event": "next", "data": _result(r)}
            yield {"event": "complete", "data": ""}

        return veil.EventSourceResponse(events())
    return _result(schema.execute_sync(query, variable_values=variables))


if __name__ == "__main__":
    veil.run(listen_cleartext=os.environ.get("VEIL_LISTEN", "127.0.0.1:8000"), overrides={"logging.level": "warn"})
