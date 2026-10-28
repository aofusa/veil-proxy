"""GraphQL with strawberry (``pip install "veil-api-kit[graphql]"``).

.. code-block:: python

    import strawberry
    from veil_kit.graphql import mount

    schema = strawberry.Schema(query=Query, mutation=Mutation, subscription=Subscription)
    mount(app, "/graphql", schema)

One path serves ``POST`` (JSON, batches too) and ``GET`` (query string), GraphQL over SSE for subscriptions
(``Accept: text/event-stream``), WebSocket subscriptions (``graphql-transport-ws`` and the older ``graphql-ws``) and
GraphiQL for a browser ``GET`` (``graphiql=False`` turns it off; the page loads its scripts from a CDN).
``context_getter(request)`` builds the context passed to resolvers (default: ``{"request": request}``).
"""

# F-268: examples/api/python-graphql（F-210）を取り込み、WebSocket（F-263）のサブスクリプションを足した

import asyncio
import json

import veil

__all__ = ["mount", "GraphQLRoutes"]

_GRAPHQL_RESPONSE = "application/graphql-response+json"


def _result(r):
    out = {"data": r.data}
    if r.errors:
        out["errors"] = [e.formatted if hasattr(e, "formatted") else {"message": str(e)} for e in r.errors]
    if getattr(r, "extensions", None):
        out["extensions"] = r.extensions
    return out


def _graphiql_html(path):
    try:
        from strawberry.http.ide import get_graphql_ide_html  # noqa: PLC0415

        return get_graphql_ide_html(graphql_ide="graphiql")
    except ImportError:
        pass
    try:
        from strawberry.utils.graphiql import get_graphiql_html  # noqa: PLC0415

        return get_graphiql_html()
    except ImportError:
        pass
    return (
        "<!doctype html><html><head><title>GraphiQL</title>"
        '<link rel="stylesheet" href="https://unpkg.com/graphiql@3/graphiql.min.css"></head>'
        '<body style="margin:0"><div id="g" style="height:100vh"></div>'
        '<script src="https://unpkg.com/react@18/umd/react.production.min.js"></script>'
        '<script src="https://unpkg.com/react-dom@18/umd/react-dom.production.min.js"></script>'
        '<script src="https://unpkg.com/graphiql@3/graphiql.min.js"></script>'
        "<script>ReactDOM.createRoot(document.getElementById('g')).render(React.createElement(GraphiQL,"
        "{fetcher:GraphiQL.createFetcher({url:%s})}))</script></body></html>" % json.dumps(path)
    )


class GraphQLRoutes:
    """The routes of one schema (created by :func:`mount`)."""

    def __init__(self, schema, path, graphiql=True, context_getter=None, keep_alive=None):
        self.schema = schema
        self.path = path
        self.graphiql = graphiql
        self.context_getter = context_getter or (lambda request: {"request": request})
        self.keep_alive = keep_alive

    async def _execute(self, body, request):
        ctx = self.context_getter(request)
        if isinstance(ctx, asyncio.Future) or asyncio.iscoroutine(ctx):
            ctx = await ctx
        r = await self.schema.execute(body.get("query") or "", variable_values=body.get("variables"),
                                      operation_name=body.get("operationName"), context_value=ctx)
        return _result(r)

    async def _subscribe(self, body, request):
        ctx = self.context_getter(request)
        if asyncio.iscoroutine(ctx):
            ctx = await ctx
        r = await self.schema.subscribe(body.get("query") or "", variable_values=body.get("variables"),
                                        operation_name=body.get("operationName"), context_value=ctx)
        if hasattr(r, "__aiter__"):
            async for x in r:
                yield _result(x)
        else:
            yield _result(r)

    async def http(self, request: veil.Request):
        """``GET`` / ``POST`` on the GraphQL path."""
        accept = request.headers.get("accept") or ""
        if request.method in ("GET", "HEAD"):
            q = request.query_params
            if not q.get("query"):
                if self.graphiql and "text/html" in accept:
                    return veil.HTMLResponse(_graphiql_html(self.path))
                raise veil.HTTPException(400, "missing query")
            try:
                variables = json.loads(q["variables"]) if q.get("variables") else None
            except ValueError:
                raise veil.HTTPException(400, "variables is not valid JSON")
            body = {"query": q.get("query"), "variables": variables, "operationName": q.get("operationName")}
        else:
            try:
                body = json.loads(request.body or b"null")
            except ValueError:
                raise veil.HTTPException(400, "the body is not valid JSON")
        if "text/event-stream" in accept:
            if not isinstance(body, dict):
                raise veil.HTTPException(400, "subscriptions over SSE take one operation")

            async def events():
                async for r in self._subscribe(body, request):
                    yield {"event": "next", "data": r}
                yield {"event": "complete", "data": ""}

            return veil.EventSourceResponse(events())
        if isinstance(body, list):
            out = [await self._execute(b, request) for b in body if isinstance(b, dict)]
        elif isinstance(body, dict):
            out = await self._execute(body, request)
        else:
            raise veil.HTTPException(400, "a GraphQL request is a JSON object")
        media = _GRAPHQL_RESPONSE if _GRAPHQL_RESPONSE in accept else "application/json"
        return veil.Response(json.dumps(out, separators=(",", ":")), 200, media_type=media)

    async def websocket(self, websocket: veil.WebSocket):
        """Subscriptions over WebSocket (``graphql-transport-ws`` and ``graphql-ws``)."""
        offered = websocket.subprotocols
        proto = next((p for p in ("graphql-transport-ws", "graphql-ws") if p in offered), None)
        if proto is None:
            await websocket.close(4406)
            return
        await websocket.accept(subprotocol=proto)
        legacy = proto == "graphql-ws"
        ops = {}
        acked = False

        async def run(op_id, payload):
            try:
                async for r in self._subscribe(payload, websocket):
                    await websocket.send_json({"type": "data" if legacy else "next", "id": op_id, "payload": r})
                await websocket.send_json({"type": "complete", "id": op_id})
            except asyncio.CancelledError:
                raise
            except Exception as e:  # noqa: BLE001
                await websocket.send_json({"type": "error", "id": op_id, "payload": [{"message": str(e)}]})
            finally:
                ops.pop(op_id, None)

        try:
            while True:
                try:
                    msg = await websocket.receive_json()
                except veil.WebSocketDisconnect:
                    break
                except (ValueError, TypeError):
                    await websocket.close(4400, "invalid message")
                    break
                t = msg.get("type") if isinstance(msg, dict) else None
                if t == "connection_init":
                    if acked and not legacy:
                        await websocket.close(4429, "Too many initialisation requests")
                        break
                    acked = True
                    await websocket.send_json({"type": "connection_ack"})
                elif t == "ping":
                    await websocket.send_json({"type": "pong", "payload": msg.get("payload")})
                elif t == "pong":
                    pass
                elif t in ("subscribe", "start"):
                    if not acked:
                        await websocket.close(4401, "Unauthorized")
                        break
                    op_id = msg.get("id")
                    if op_id in ops:
                        await websocket.close(4409, "Subscriber for %s already exists" % op_id)
                        break
                    ops[op_id] = asyncio.get_running_loop().create_task(run(op_id, msg.get("payload") or {}))
                elif t in ("complete", "stop"):
                    task = ops.pop(msg.get("id"), None)
                    if task is not None:
                        task.cancel()
                elif t == "connection_terminate":
                    await websocket.close(1000)
                    break
                else:
                    await websocket.close(4400, "Unknown message type")
                    break
        finally:
            for task in list(ops.values()):
                task.cancel()


def mount(app, path, schema, *, graphiql=True, context_getter=None, websocket=True):
    """Serve ``schema`` (a ``strawberry.Schema``) at ``path`` on ``app``; returns the :class:`GraphQLRoutes`."""
    routes = GraphQLRoutes(schema, path, graphiql, context_getter)

    async def graphql_http(request: veil.Request):
        return await routes.http(request)

    graphql_http.__name__ = "graphql" + path.replace("/", "_").replace("{", "").replace("}", "")
    app.route(path, methods=["GET", "POST"], tags=["graphql"], summary="GraphQL endpoint")(graphql_http)
    if websocket:
        async def graphql_ws(websocket: veil.WebSocket):
            await routes.websocket(websocket)

        graphql_ws.__name__ = graphql_http.__name__ + "_ws"
        # 同じパスの GET と WebSocket は、サーバが Upgrade の有無で振り分ける
        app.websocket(path, subprotocols=["graphql-transport-ws", "graphql-ws"], include_in_schema=False)(graphql_ws)
    return routes
